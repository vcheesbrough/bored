use tracing_subscriber::{
    EnvFilter, Layer, Registry, layer::SubscriberExt, util::SubscriberInitExt,
};

use crate::config::ObservabilityConfig;

pub struct ObservabilityGuard {
    _loki_task: Option<tokio::task::JoinHandle<()>>,
}

/// Initialise structured logging. Call once as the first statement in main(),
/// with the already-loaded, already-validated `ObservabilityConfig`. The
/// returned guard must be kept alive for the process lifetime — dropping it
/// detaches the Loki background task (the task continues running). Shutdown
/// ordering is not guaranteed, so buffered log events may be lost at process exit.
pub fn init(config: &ObservabilityConfig) -> ObservabilityGuard {
    // Burned-in release tag (see shared::app_version); APP_VERSION can override.
    let version =
        crate::config::app_version_override().unwrap_or_else(|| shared::app_version().to_string());

    // Each layer gets its own EnvFilter via .with_filter() so that
    // register_callsite interest is correctly computed per-layer. A shared
    // EnvFilter pushed into the Vec doesn't work: Vec<Layer> takes the most
    // permissive register_callsite interest across all sub-layers, so the fmt
    // layer's Interest::always() would bypass the filter entirely.
    let make_filter =
        || EnvFilter::try_new(&config.log_level).unwrap_or_else(|_| EnvFilter::new("info"));

    let mut layers: Vec<Box<dyn Layer<Registry> + Send + Sync>> = Vec::new();

    // Console layer: JSON in production, pretty otherwise
    let fmt: Box<dyn Layer<Registry> + Send + Sync> = if config.environment == "production" {
        Box::new(
            tracing_subscriber::fmt::layer()
                .json()
                .flatten_event(true)
                .with_filter(make_filter()),
        )
    } else {
        Box::new(
            tracing_subscriber::fmt::layer()
                .pretty()
                .with_filter(make_filter()),
        )
    };
    layers.push(fmt);

    // Loki layer. `config.loki_url` is already a validated `url::Url` — a
    // malformed value is rejected at config load time (fail-closed startup
    // error), not here.
    let loki_task = if let Some(url) = config.loki_url.clone() {
        let (loki_layer, controller) = tracing_loki::builder()
            .label("app", &config.service_name)
            .expect("observability.service-name contains characters invalid in a Loki label value")
            .label("env", &config.environment)
            .expect("observability.environment contains characters invalid in a Loki label value")
            .label("version", version)
            .unwrap()
            .build_url(url)
            .expect("failed to build Loki layer");
        layers.push(Box::new(loki_layer.with_filter(make_filter())));
        Some(tokio::spawn(controller))
    } else {
        None
    };

    tracing_subscriber::registry().with(layers).init();

    ObservabilityGuard {
        _loki_task: loki_task,
    }
}

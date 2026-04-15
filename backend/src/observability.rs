use tracing_subscriber::{
    layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer, Registry,
};

pub struct ObservabilityGuard {
    _loki_task: Option<tokio::task::JoinHandle<()>>,
}

/// Initialise structured logging. Call once as the first statement in main().
/// The returned guard must be kept alive for the process lifetime — dropping it
/// detaches the Loki background task (the task continues running). Shutdown
/// ordering is not guaranteed, so buffered log events may be lost at process exit.
pub fn init() -> ObservabilityGuard {
    let log_level = std::env::var("LOG_LEVEL").unwrap_or_else(|_| "info".into());
    let app_env = std::env::var("APP_ENV").unwrap_or_else(|_| "development".into());
    let loki_url = std::env::var("LOKI_URL").ok();
    let version = env!("CARGO_PKG_VERSION");

    // All layers go into a single Vec<Box<dyn Layer<Registry>>> so the
    // subscriber type stays as `Layered<Vec<...>, Registry>` throughout.
    // EnvFilter must be in the same Vec — if it were added via a separate
    // .with() call the subscriber type becomes Layered<EnvFilter, Registry>
    // and the Vec (typed against bare Registry) no longer satisfies the bound.
    let mut layers: Vec<Box<dyn Layer<Registry> + Send + Sync>> = Vec::new();

    let filter = EnvFilter::try_new(&log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    layers.push(Box::new(filter));

    // Console layer: JSON in production, pretty otherwise
    let fmt: Box<dyn Layer<Registry> + Send + Sync> = if app_env == "production" {
        Box::new(tracing_subscriber::fmt::layer().json().flatten_event(true))
    } else {
        Box::new(tracing_subscriber::fmt::layer().pretty())
    };
    layers.push(fmt);

    // Loki layer
    let loki_task = if let Some(url_str) = loki_url {
        let url = url::Url::parse(&url_str)
            .unwrap_or_else(|e| panic!("LOKI_URL '{url_str}' is not a valid URL: {e}"));
        let (loki_layer, controller) = tracing_loki::builder()
            .label("app", "bored")
            .expect("hardcoded label \"app\" is invalid — should never happen")
            .label("env", &app_env)
            .expect("APP_ENV contains characters invalid in a Loki label value")
            .label("version", version)
            .unwrap()
            .build_url(url)
            .expect("failed to build Loki layer");
        layers.push(Box::new(loki_layer));
        Some(tokio::spawn(controller))
    } else {
        None
    };

    tracing_subscriber::registry().with(layers).init();

    ObservabilityGuard {
        _loki_task: loki_task,
    }
}

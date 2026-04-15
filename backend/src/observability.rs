use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer, Registry};

pub struct ObservabilityGuard {
    _loki_task: Option<tokio::task::JoinHandle<()>>,
}

/// Initialise structured logging. Call once as the first statement in main().
/// The returned guard must be kept alive for the process lifetime — dropping it
/// shuts down the Loki background task.
pub fn init() -> ObservabilityGuard {
    let log_level = std::env::var("LOG_LEVEL").unwrap_or_else(|_| "info".into());
    let app_env = std::env::var("APP_ENV").unwrap_or_else(|_| "development".into());
    let loki_url = std::env::var("LOKI_URL").ok();
    let version = env!("CARGO_PKG_VERSION");

    let filter = EnvFilter::try_new(&log_level).unwrap_or_else(|_| EnvFilter::new("info"));

    let mut layers: Vec<Box<dyn Layer<Registry> + Send + Sync>> = Vec::new();

    // Console layer: JSON in production, pretty otherwise
    let fmt: Box<dyn Layer<Registry> + Send + Sync> = if app_env == "production" {
        Box::new(tracing_subscriber::fmt::layer().json().flatten_event(true))
    } else {
        Box::new(tracing_subscriber::fmt::layer().pretty())
    };
    layers.push(fmt);

    // Optional Loki layer — omitted when LOKI_URL is unset
    let loki_task = if let Some(url_str) = loki_url {
        let url = url::Url::parse(&url_str).expect("LOKI_URL is not a valid URL");
        let (loki_layer, controller) = tracing_loki::builder()
            .label("app", "bored")
            .unwrap()
            .label("env", &app_env)
            .unwrap()
            .label("version", version)
            .unwrap()
            .build_url(url)
            .expect("failed to build Loki layer");
        layers.push(Box::new(loki_layer));
        Some(tokio::spawn(controller))
    } else {
        None
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(layers)
        .init();

    ObservabilityGuard { _loki_task: loki_task }
}

use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use crate::config::ObservabilityConfig;

/// Initialise structured logging. Call once as the first statement in main().
///
/// Logs go to stdout and nowhere else. The homelab's Alloy collects the
/// container's Docker log stream and ships it to Loki, attaching the labels
/// from `deploy/docker-compose.yml` (`observability.service.name` →
/// `service_name`, `observability.deployment.environment` →
/// `deployment_environment`) plus `log_source="docker"`, and the image's OCI
/// `version`/`revision` labels as per-line structured metadata. That is the one
/// path in — the process used to *also* push straight to Loki's API via
/// `tracing-loki`, which stored every line a second time under a differently
/// labelled stream (card #412).
///
/// One consequence worth knowing: an event reaches Loki as soon as it is
/// written to stdout, so there is no buffered backlog to lose at exit and no
/// background task to keep alive — hence nothing to return. The old
/// `ObservabilityGuard` existed only to own the `tracing-loki` push task.
pub fn init(config: &ObservabilityConfig) {
    // JSON in every environment, deployed or local. Alloy ships one Loki entry
    // per physical line, and `pretty` spreads a single event over several of
    // them; nothing reads this stream raw any more, so the old
    // `environment == "production"` switch to `pretty` is deleted rather than
    // re-pointed at the new `"prod"` value.
    //
    // `flatten_event` lifts the event's own fields to the top level of the JSON
    // object instead of nesting them under `"fields"`, which is what makes them
    // addressable as `| json | field="…"` in a Loki query.
    let fmt = tracing_subscriber::fmt::layer().json().flatten_event(true);

    // `try_new` rather than `new`: an unparseable filter directive is a config
    // typo, not a reason to refuse to start, so fall back to `info`. (Only a
    // malformed `observability.log-level` reaches here — the leaf itself is
    // required, and validated as non-empty at load time.)
    let filter = EnvFilter::try_new(&config.log_level).unwrap_or_else(|_| EnvFilter::new("info"));

    // A single layer, so the filter goes on the subscriber as a whole. (With
    // several layers it would have to be attached per-layer via `.with_filter()`
    // instead: a `Vec<Layer>` reports the most permissive `register_callsite`
    // interest across its members, and the fmt layer's `Interest::always()`
    // would then bypass a subscriber-level filter entirely.)
    tracing_subscriber::registry().with(fmt).with(filter).init();
}

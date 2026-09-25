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
    subscriber(config, std::io::stdout).init();
}

/// The subscriber `init` installs, built over an injectable writer.
///
/// Split out from [`init`] purely so it can be tested: `init` installs the
/// global default (once per process, irreversibly) and writes to the real
/// stdout, neither of which a test can observe. Taking the writer as a
/// parameter lets a test collect the bytes and run the subscriber under
/// `tracing::subscriber::with_default` instead.
fn subscriber<W>(config: &ObservabilityConfig, writer: W) -> impl tracing::Subscriber
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    // JSON in every environment, deployed or local. Alloy ships one Loki entry
    // per physical line, and `pretty` spreads a single event over several of
    // them; nothing reads this stream raw any more, so the old
    // `environment == "production"` switch to `pretty` is deleted rather than
    // re-pointed at the new `"prod"` value.
    //
    // `flatten_event` lifts the event's own fields to the top level of the JSON
    // object instead of nesting them under `"fields"`, which is what makes them
    // addressable as `| json | field="…"` in a Loki query.
    let fmt = tracing_subscriber::fmt::layer()
        .json()
        .flatten_event(true)
        .with_writer(writer);

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
    tracing_subscriber::registry().with(fmt).with(filter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::{Arc, Mutex};

    /// A `MakeWriter` that appends everything written to a shared buffer, so a
    /// test can read back exactly what would have gone to stdout.
    #[derive(Clone, Default)]
    struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for CapturedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("writer mutex poisoned").extend(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedWriter {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn config(log_level: &str) -> ObservabilityConfig {
        ObservabilityConfig {
            environment: "test".to_string(),
            log_level: log_level.to_string(),
            branch: None,
        }
    }

    /// Run `body` against a subscriber built at `log_level` and return what it
    /// wrote.
    fn capture(log_level: &str, body: impl FnOnce()) -> String {
        let writer = CapturedWriter::default();
        let buffer = Arc::clone(&writer.0);
        tracing::subscriber::with_default(subscriber(&config(log_level), writer), body);
        let bytes = buffer.lock().expect("writer mutex poisoned").clone();
        String::from_utf8(bytes).expect("log output should be UTF-8")
    }

    /// The property the whole card rests on: Alloy turns each *physical line*
    /// of the container's stdout into one Loki entry, so an event that spans
    /// several lines (as the old `pretty` format did outside prod) arrives as
    /// several unrelated entries. Asserting "exactly one line, and it parses as
    /// JSON" is what would fail if a `pretty` branch were ever reintroduced.
    #[test]
    fn an_event_is_written_as_exactly_one_line_of_json() {
        let output = capture("info", || {
            tracing::info!(card = 412, "logs reach loki via stdout");
        });

        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 1, "expected a single line, got: {output:?}");

        let entry: serde_json::Value =
            serde_json::from_str(lines[0]).expect("the line should be valid JSON");
        // `flatten_event` puts the message and the event's own fields at the top
        // level rather than under a nested "fields" object — that flattening is
        // what makes them addressable in a Loki `| json` query.
        assert_eq!(entry["message"], "logs reach loki via stdout");
        assert_eq!(entry["card"], 412);
        assert_eq!(entry["level"], "INFO");
    }

    /// `log-level` still gates events now that the filter sits on the
    /// subscriber rather than on the layer.
    ///
    /// Worth pinning rather than assuming: a `Vec` of layers reports the most
    /// permissive `register_callsite` interest across its members, so the fmt
    /// layer's `Interest::always()` would defeat a subscriber-level filter —
    /// which is exactly why the previous multi-layer version attached the
    /// filter per-layer with `.with_filter()`. It holds here only because there
    /// is a single layer, so a future second layer must restore the per-layer
    /// form.
    #[test]
    fn events_below_the_configured_level_are_suppressed() {
        let output = capture("warn", || {
            tracing::info!("should not appear");
            tracing::warn!("should appear");
        });

        assert!(
            !output.contains("should not appear"),
            "info survived a warn filter: {output:?}"
        );
        assert!(
            output.contains("should appear"),
            "warn was dropped by a warn filter: {output:?}"
        );
    }
}

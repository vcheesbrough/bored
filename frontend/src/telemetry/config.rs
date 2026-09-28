//! Turning what `/api/info` says into "export, and where" or "never initialise
//! OTLP" (card #416).
//!
//! `client-export.md`, *Telemetry configuration comes from the product*: the
//! client has **no default endpoint**. No compiled-in fallback, nothing derived
//! from the page's own origin, no localhost. Absent configuration is
//! disabled, unambiguously — and so is a configuration this code cannot use.

/// The launch sequence's spans are held this long waiting for configuration,
/// then discarded if none has arrived ("never hold them for the whole session
/// in hope"). Three heartbeats' worth: the first `/api/info` normally answers
/// within a second of load.
pub const LAUNCH_BUFFER_MS: f64 = 30_000.0;

/// What the SPA does about telemetry for the rest of the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Export to this base URL (no trailing slash; OTLP paths are appended).
    Enabled { endpoint: String },
    /// Do not initialise OTLP. Anything buffered is discarded.
    Disabled,
}

/// Decide from the `telemetry` block of `/api/info`. `None` — the key absent,
/// an older server — is disabled.
pub fn decide(config: Option<&shared::ClientTelemetryConfig>) -> Decision {
    // `let … else` binds on the happy path and bails out otherwise, which
    // keeps every refusal a flat early return.
    let Some(config) = config else {
        return Decision::Disabled;
    };
    if !config.enabled {
        return Decision::Disabled;
    }
    match normalise_endpoint(&config.endpoint) {
        Some(endpoint) => Decision::Enabled { endpoint },
        None => Decision::Disabled,
    }
}

/// Accept an absolute `https://` (or `http://`, for a test rig) base URL with a
/// host and nothing after the path, and strip any trailing `/` so appending
/// `/v1/traces` yields one slash. Anything else — relative, empty, a query
/// string — is refused rather than guessed at.
fn normalise_endpoint(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let rest = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))?;
    // The authority is everything up to the first `/`; it must be non-empty.
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty() || trimmed.contains(['?', '#', ' ']) {
        return None;
    }
    Some(trimmed.trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::ClientTelemetryConfig;

    fn config(enabled: bool, endpoint: &str) -> ClientTelemetryConfig {
        ClientTelemetryConfig {
            enabled,
            endpoint: endpoint.to_string(),
        }
    }

    #[test]
    fn no_configuration_means_no_telemetry() {
        assert_eq!(decide(None), Decision::Disabled);
    }

    #[test]
    fn switched_off_means_no_telemetry_even_with_an_endpoint() {
        assert_eq!(
            decide(Some(&config(false, "https://bored.example"))),
            Decision::Disabled
        );
    }

    #[test]
    fn enabled_with_a_usable_endpoint_exports_there() {
        assert_eq!(
            decide(Some(&config(true, "https://bored.example/"))),
            Decision::Enabled {
                endpoint: "https://bored.example".to_string()
            }
        );
        assert_eq!(
            decide(Some(&config(true, "http://app:8443/otlp"))),
            Decision::Enabled {
                endpoint: "http://app:8443/otlp".to_string()
            }
        );
    }

    #[test]
    fn an_unusable_endpoint_is_disabled_not_guessed() {
        for endpoint in [
            "",
            "   ",
            "/v1",
            "bored.example",
            "https://",
            "https:///v1",
            "ftp://bored.example",
            "https://bored.example/?x=1",
        ] {
            assert_eq!(
                decide(Some(&config(true, endpoint))),
                Decision::Disabled,
                "{endpoint:?}"
            );
        }
    }

    #[test]
    fn older_servers_reply_parses_as_disabled() {
        // `/api/info` from a server built before card #416 has no `telemetry`
        // key at all; the shared type's `#[serde(default)]` must make that
        // `None`, which decides to Disabled.
        let info: shared::AppInfo =
            serde_json::from_str(r#"{"version":"1.63.0","env":"dev"}"#).unwrap();
        assert_eq!(decide(info.telemetry.as_ref()), Decision::Disabled);
    }
}

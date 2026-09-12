//! Per-DTO config tests.
//!
//! These drive the real layering (in-memory defaults + the `BORED__` environment
//! source) but inject the environment map explicitly rather than mutating the
//! process environment, so the tests stay deterministic under parallel execution.

use std::collections::HashMap;

use serial_test::serial;

use super::*;

/// Build a `config::Config` from the real defaults plus an injected env map,
/// exactly as `build_config` does minus the sovereign-config layer.
fn cfg(entries: &[(&str, &str)]) -> Config {
    let source: HashMap<String, String> = entries
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect();
    apply_defaults(Config::builder())
        .expect("defaults should apply")
        // Wrapped exactly as `build_config` wraps it, so tests exercise trimming.
        .add_source(Trimmed(env_source().source(Some(source))))
        .build()
        .expect("config should build")
}

/// As [`cfg`], but with a stand-in for the sovereign-config layer between the
/// defaults and the env source — the same order, and the same `Trimmed` wrapper,
/// that `build_config` uses when an access URL is present. Keys are the canonical
/// dotted paths the real source emits (`observability.service-name`).
fn cfg_with_sovereign(sovereign: &[(&str, &str)], env: &[(&str, &str)]) -> Config {
    let mut layer = Config::builder();
    for (key, value) in sovereign {
        layer = layer
            .set_override(*key, *value)
            .expect("sovereign fixture leaf should set");
    }
    let layer = layer.build().expect("sovereign fixture should build");

    let source: HashMap<String, String> = env
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect();
    apply_defaults(Config::builder())
        .expect("defaults should apply")
        .add_source(Trimmed(layer))
        .add_source(Trimmed(env_source().source(Some(source))))
        .build()
        .expect("config should build")
}

fn oidc_env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("BORED__OIDC__ISSUER-URL", "https://auth.example/o/bored/"),
        ("BORED__OIDC__CLIENT-ID", "bored-browser"),
        ("BORED__OIDC__CLIENT-SECRET", "shhh"),
        (
            "BORED__OIDC__REDIRECT-URI",
            "https://bored.example/auth/callback",
        ),
        ("BORED__OIDC__REQUIRED-SCOPE", "bored:prod:access"),
    ]
}

/// 64 zero-padded bytes, base64-encoded — a structurally valid cookie key (the
/// same fixture value the e2e suite uses). `SessionConfig::validate` requires
/// exactly 64 decoded bytes, so tests that need a *loadable* key use this
/// rather than an arbitrary string.
const VALID_COOKIE_KEY: &str =
    "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWYwMTIzNDU2Nzg5YWJjZGVmMDEyMzQ1Njc4OWFiY2RlZg==";

fn session_env() -> Vec<(&'static str, &'static str)> {
    vec![("BORED__SESSION__COOKIE-KEY", VALID_COOKIE_KEY)]
}

fn with(
    base: Vec<(&'static str, &'static str)>,
    extra: &[(&'static str, &'static str)],
) -> Vec<(&'static str, &'static str)> {
    let mut all = base;
    all.extend_from_slice(extra);
    all
}

// ---------------------------------------------------------------------------
// oidc (required group)
// ---------------------------------------------------------------------------

#[test]
fn oidc_maps_kebab_keys_into_rich_types() {
    let config = cfg(&with(
        oidc_env(),
        &[
            (
                "BORED__OIDC__END-SESSION-URL",
                "https://auth.example/logout",
            ),
            (
                "BORED__OIDC__MCP__ISSUER-URL",
                "https://auth.example/o/mcp/",
            ),
            ("BORED__OIDC__MCP__CLIENT-ID", "bored-mcp"),
        ],
    ));
    let oidc: OidcConfig = load_group(&config, "oidc").expect("oidc group should load");

    assert_eq!(oidc.issuer_url, "https://auth.example/o/bored/");
    assert_eq!(oidc.client_id, "bored-browser");
    assert_eq!(oidc.client_secret, "shhh");
    assert_eq!(oidc.redirect_uri, "https://bored.example/auth/callback");
    assert_eq!(oidc.required_scope, "bored:prod:access");
    assert_eq!(
        oidc.end_session_url.as_deref(),
        Some("https://auth.example/logout")
    );
    assert_eq!(
        oidc.mcp.issuer_url.as_deref(),
        Some("https://auth.example/o/mcp/")
    );
    assert_eq!(oidc.mcp.client_id.as_deref(), Some("bored-mcp"));
}

#[test]
fn oidc_blank_optional_leaves_are_treated_as_absent() {
    let config = cfg(&oidc_env());
    let oidc: OidcConfig = load_group(&config, "oidc").expect("should load");

    assert!(oidc.end_session_url.is_none());
    assert!(oidc.mcp.issuer_url.is_none());
    assert!(oidc.mcp.client_id.is_none());
}

#[test]
fn oidc_missing_client_id_is_rejected() {
    let entries: Vec<_> = oidc_env()
        .into_iter()
        .filter(|(key, _)| *key != "BORED__OIDC__CLIENT-ID")
        .collect();
    let error = load_group::<OidcConfig>(&cfg(&entries), "oidc")
        .expect_err("missing client-id should be rejected");
    match error {
        ConfigError::Load { ref group, .. } => assert_eq!(group, "oidc"),
        other => panic!("expected Load, got: {other}"),
    }
}

#[test]
fn oidc_blank_client_secret_is_rejected_without_leaking_it() {
    let config = cfg(&with(oidc_env(), &[("BORED__OIDC__CLIENT-SECRET", "   ")]));
    let error =
        load_group::<OidcConfig>(&config, "oidc").expect_err("blank secret should be rejected");
    assert_eq!(
        error.to_string(),
        "invalid config `oidc.client-secret`: must not be empty"
    );
}

#[test]
fn oidc_blank_required_scope_is_rejected() {
    let config = cfg(&with(oidc_env(), &[("BORED__OIDC__REQUIRED-SCOPE", "   ")]));
    let error =
        load_group::<OidcConfig>(&config, "oidc").expect_err("blank scope should be rejected");
    assert_eq!(
        error.to_string(),
        "invalid config `oidc.required-scope`: must not be empty"
    );
}

// ---------------------------------------------------------------------------
// oidc (optional-group matrix — bored's auth-disabled dev mode)
// ---------------------------------------------------------------------------

#[test]
fn load_optional_oidc_returns_none_when_issuer_is_absent() {
    let config = cfg(&[]);
    assert!(load_optional_oidc(&config)
        .expect("should not error")
        .is_none());
}

#[test]
fn load_optional_oidc_returns_none_when_issuer_is_blank() {
    // Mirrors `deploy/docker-compose.yml`'s `${OIDC_ISSUER_URL:-}` forwarding an
    // unset host var as an empty string rather than an absent one.
    let config = cfg(&[("BORED__OIDC__ISSUER-URL", "")]);
    assert!(load_optional_oidc(&config)
        .expect("should not error")
        .is_none());
}

#[test]
fn load_optional_oidc_returns_some_when_issuer_is_present_and_complete() {
    let config = cfg(&oidc_env());
    let oidc = load_optional_oidc(&config)
        .expect("should not error")
        .expect("issuer present should yield Some");
    assert_eq!(oidc.client_id, "bored-browser");
}

#[test]
fn load_optional_oidc_names_the_missing_field_when_issuer_present_but_incomplete() {
    let entries: Vec<_> = oidc_env()
        .into_iter()
        .filter(|(key, _)| *key != "BORED__OIDC__REDIRECT-URI")
        .collect();
    let error = load_optional_oidc(&cfg(&entries))
        .expect_err("incomplete oidc with issuer present should be a hard error");
    match error {
        ConfigError::Load { ref group, .. } => assert_eq!(group, "oidc"),
        other => panic!("expected Load, got: {other}"),
    }
}

// ---------------------------------------------------------------------------
// session (required whenever oidc is enabled)
// ---------------------------------------------------------------------------

#[test]
fn session_maps_cookie_key() {
    let config = cfg(&session_env());
    let session: SessionConfig = load_group(&config, "session").expect("should load");
    assert_eq!(session.cookie_key, VALID_COOKIE_KEY);
}

#[test]
fn session_blank_cookie_key_is_rejected() {
    let config = cfg(&[("BORED__SESSION__COOKIE-KEY", "   ")]);
    let error = load_group::<SessionConfig>(&config, "session")
        .expect_err("blank cookie key should be rejected");
    assert_eq!(
        error.to_string(),
        "invalid config `session.cookie-key`: must not be empty"
    );
}

#[test]
fn session_cookie_key_that_is_not_valid_base64_is_rejected() {
    let config = cfg(&[("BORED__SESSION__COOKIE-KEY", "not-valid-base64!!!")]);
    let error = load_group::<SessionConfig>(&config, "session")
        .expect_err("non-base64 cookie key should be rejected");
    assert!(error.to_string().contains("session.cookie-key"));
}

#[test]
fn session_cookie_key_of_the_wrong_decoded_length_is_rejected() {
    let too_short = base64::engine::general_purpose::STANDARD.encode([9_u8; 32]);
    let config = cfg(&[("BORED__SESSION__COOKIE-KEY", too_short.as_str())]);
    let error = load_group::<SessionConfig>(&config, "session")
        .expect_err("32-byte key should be rejected");
    assert!(error.to_string().contains("64 bytes"));
}

// ---------------------------------------------------------------------------
// observability
// ---------------------------------------------------------------------------

#[test]
fn observability_defaults_apply_when_only_environment_is_set() {
    let config = cfg(&[("BORED__OBSERVABILITY__ENVIRONMENT", "dev")]);
    let observability: ObservabilityConfig =
        load_group(&config, "observability").expect("should load");

    assert_eq!(observability.environment, "dev");
    assert_eq!(observability.log_level, "info");
    assert_eq!(observability.service_name, "bored");
    assert!(observability.loki_url.is_none());
}

#[test]
fn observability_environment_overrides_defaults() {
    let config = cfg(&[
        ("BORED__OBSERVABILITY__ENVIRONMENT", "production"),
        ("BORED__OBSERVABILITY__LOG-LEVEL", "warn"),
        ("BORED__OBSERVABILITY__SERVICE-NAME", "bored-prod"),
        ("BORED__OBSERVABILITY__LOKI-URL", "http://monitor-loki:3100"),
    ]);
    let observability: ObservabilityConfig =
        load_group(&config, "observability").expect("should load");

    assert_eq!(observability.environment, "production");
    assert_eq!(observability.log_level, "warn");
    assert_eq!(observability.service_name, "bored-prod");
    assert!(observability.loki_url.is_some());
}

#[test]
fn observability_blank_loki_url_disables_export() {
    let config = cfg(&[("BORED__OBSERVABILITY__LOKI-URL", "")]);
    let observability: ObservabilityConfig =
        load_group(&config, "observability").expect("should load");
    assert!(observability.loki_url.is_none());
}

#[test]
fn observability_malformed_loki_url_fails_to_deserialize() {
    let config = cfg(&[("BORED__OBSERVABILITY__LOKI-URL", "not a url")]);
    load_group::<ObservabilityConfig>(&config, "observability")
        .expect_err("malformed URL should be rejected");
}

#[test]
fn observability_blank_environment_override_is_rejected_by_validate() {
    let config = cfg(&[("BORED__OBSERVABILITY__ENVIRONMENT", "   ")]);
    let error = load_group::<ObservabilityConfig>(&config, "observability")
        .expect_err("blank environment should be rejected");
    assert_eq!(
        error.to_string(),
        "invalid config `observability.environment`: must not be empty"
    );
}

// ---------------------------------------------------------------------------
// server
// ---------------------------------------------------------------------------

#[test]
fn server_defaults_to_http_3000_with_dist_and_data_paths() {
    let config = cfg(&[]);
    let server: ServerConfig = load_group(&config, "server").expect("should load");

    assert_eq!(server.http_port, 3000);
    assert_eq!(server.static_dir, "./dist");
    assert_eq!(server.database_path, "/data/bored.db");
    assert!(server.tls_pair().is_none());
}

#[test]
fn server_maps_kebab_keys_and_coerces_port() {
    let config = cfg(&[
        ("BORED__SERVER__HTTP-PORT", "9001"),
        ("BORED__SERVER__TLS-CERT", "/app/cert.pem"),
        ("BORED__SERVER__TLS-KEY", "/app/key.pem"),
        ("BORED__SERVER__STATIC-DIR", "/app/dist"),
        ("BORED__SERVER__DATABASE-PATH", "/data/other.db"),
    ]);
    let server: ServerConfig = load_group(&config, "server").expect("should load");

    assert_eq!(server.http_port, 9001);
    assert_eq!(server.tls_pair(), Some(("/app/cert.pem", "/app/key.pem")));
    assert_eq!(server.static_dir, "/app/dist");
    assert_eq!(server.database_path, "/data/other.db");
}

#[test]
fn server_non_numeric_port_fails_to_deserialize() {
    let config = cfg(&[("BORED__SERVER__HTTP-PORT", "not-a-port")]);
    load_group::<ServerConfig>(&config, "server").expect_err("non-numeric port should be rejected");
}

#[test]
fn server_tls_cert_without_key_is_rejected() {
    let config = cfg(&[("BORED__SERVER__TLS-CERT", "/app/cert.pem")]);
    let error = load_group::<ServerConfig>(&config, "server")
        .expect_err("lone tls-cert should be rejected");
    assert!(error.to_string().contains("server.tls-key"));
}

#[test]
fn server_tls_key_without_cert_is_rejected() {
    let config = cfg(&[("BORED__SERVER__TLS-KEY", "/app/key.pem")]);
    let error =
        load_group::<ServerConfig>(&config, "server").expect_err("lone tls-key should be rejected");
    assert!(error.to_string().contains("server.tls-cert"));
}

#[test]
fn server_blank_tls_is_treated_as_absent() {
    let config = cfg(&[
        ("BORED__SERVER__TLS-CERT", ""),
        ("BORED__SERVER__TLS-KEY", ""),
    ]);
    let server: ServerConfig = load_group(&config, "server").expect("should load");
    assert!(server.tls_pair().is_none());
}

// ---------------------------------------------------------------------------
// trimming
// ---------------------------------------------------------------------------

#[test]
fn required_scope_with_surrounding_whitespace_is_trimmed() {
    let config = cfg(&with(
        oidc_env(),
        &[("BORED__OIDC__REQUIRED-SCOPE", "  bored:dev:access\n")],
    ));
    let oidc: OidcConfig = load_group(&config, "oidc").expect("should load");

    assert_eq!(oidc.required_scope, "bored:dev:access");
    // The comparison auth.rs performs must now succeed.
    assert!("openid profile bored:dev:access"
        .split_whitespace()
        .any(|value| value == oidc.required_scope));
}

/// Trimming is applied to every string leaf, not a curated list — including
/// secrets, where surrounding whitespace is a paste artefact rather than intent.
#[test]
fn every_string_leaf_is_trimmed_including_secrets() {
    let config = cfg(&[
        (
            "BORED__OIDC__ISSUER-URL",
            "  https://auth.example/o/bored/  ",
        ),
        ("BORED__OIDC__CLIENT-ID", "\tbored-browser\n"),
        ("BORED__OIDC__CLIENT-SECRET", "  shhh\n"),
        (
            "BORED__OIDC__REDIRECT-URI",
            " https://bored.example/auth/callback ",
        ),
        ("BORED__OIDC__REQUIRED-SCOPE", "  bored:prod:access  "),
    ]);
    let oidc: OidcConfig = load_group(&config, "oidc").expect("should load");

    assert_eq!(oidc.issuer_url, "https://auth.example/o/bored/");
    assert_eq!(oidc.client_id, "bored-browser");
    assert_eq!(oidc.client_secret, "shhh");
    assert_eq!(oidc.redirect_uri, "https://bored.example/auth/callback");
    assert_eq!(oidc.required_scope, "bored:prod:access");
}

/// Trimming must not turn a whitespace-only value into a silently accepted one:
/// it becomes empty, which `validate` still rejects by field path.
#[test]
fn whitespace_only_value_is_still_rejected() {
    let config = cfg(&with(
        oidc_env(),
        &[("BORED__OIDC__CLIENT-SECRET", "   \n ")],
    ));
    let error = load_group::<OidcConfig>(&config, "oidc").expect_err("blank secret rejected");

    match error {
        ConfigError::Invalid { ref path, .. } => assert_eq!(path, "oidc.client-secret"),
        other => panic!("expected Invalid, got: {other}"),
    }
}

// ---------------------------------------------------------------------------
// shell-safe env names
// ---------------------------------------------------------------------------

/// Canonical leaf keys are kebab-case to match sovereign-config paths, but `-` is
/// not legal in a POSIX shell variable name — `BORED__OBSERVABILITY__LOG_LEVEL=x`
/// via kebab (`LOG-LEVEL`) is parsed as a command, not an assignment. Every
/// multi-word leaf must therefore also accept the snake_case spelling.
///
/// This exercises **every** multi-word leaf across all four groups, so a new
/// field added without snake_case support fails here rather than silently
/// breaking `cargo run` for local dev.
#[test]
fn every_multi_word_leaf_accepts_a_shell_safe_snake_case_name() {
    let oidc: OidcConfig = load_group(
        &cfg(&[
            ("BORED__OIDC__ISSUER_URL", "https://auth.example/o/bored/"),
            ("BORED__OIDC__CLIENT_ID", "browser"),
            ("BORED__OIDC__CLIENT_SECRET", "shhh"),
            (
                "BORED__OIDC__REDIRECT_URI",
                "https://bored.example/auth/callback",
            ),
            ("BORED__OIDC__REQUIRED_SCOPE", "bored:prod:access"),
            (
                "BORED__OIDC__END_SESSION_URL",
                "https://auth.example/logout",
            ),
            (
                "BORED__OIDC__MCP__ISSUER_URL",
                "https://auth.example/o/mcp/",
            ),
            ("BORED__OIDC__MCP__CLIENT_ID", "bored-mcp"),
        ]),
        "oidc",
    )
    .expect("snake_case oidc leaves should load");
    assert_eq!(oidc.client_id, "browser");
    assert_eq!(oidc.required_scope, "bored:prod:access");
    assert!(oidc.end_session_url.is_some());
    assert!(oidc.mcp.issuer_url.is_some());
    assert!(oidc.mcp.client_id.is_some());

    let session: SessionConfig = load_group(
        &cfg(&[("BORED__SESSION__COOKIE_KEY", VALID_COOKIE_KEY)]),
        "session",
    )
    .expect("snake_case session leaf should load");
    assert_eq!(session.cookie_key, VALID_COOKIE_KEY);

    let observability: ObservabilityConfig = load_group(
        &cfg(&[
            ("BORED__OBSERVABILITY__ENVIRONMENT", "dev"),
            ("BORED__OBSERVABILITY__LOG_LEVEL", "debug"),
            ("BORED__OBSERVABILITY__SERVICE_NAME", "bored-snake"),
            ("BORED__OBSERVABILITY__LOKI_URL", "http://alloy:3100/"),
        ]),
        "observability",
    )
    .expect("snake_case observability leaves should load");
    assert_eq!(observability.log_level, "debug");
    assert_eq!(observability.service_name, "bored-snake");
    assert!(observability.loki_url.is_some());

    let server: ServerConfig = load_group(
        &cfg(&[
            ("BORED__SERVER__HTTP_PORT", "9001"),
            ("BORED__SERVER__TLS_CERT", "/app/cert.pem"),
            ("BORED__SERVER__TLS_KEY", "/app/key.pem"),
            ("BORED__SERVER__STATIC_DIR", "/app/dist"),
            ("BORED__SERVER__DATABASE_PATH", "/data/other.db"),
        ]),
        "server",
    )
    .expect("snake_case server leaves should load");
    assert_eq!(server.http_port, 9001);
    assert!(server.tls_pair().is_some());
    assert_eq!(server.static_dir, "/app/dist");
    assert_eq!(server.database_path, "/data/other.db");
}

/// Both spellings must reach the same field — kebab for sovereign-config/compose
/// parity, snake for shell assignment.
#[test]
fn kebab_and_snake_names_are_interchangeable() {
    let kebab: ServerConfig =
        load_group(&cfg(&[("BORED__SERVER__HTTP-PORT", "9100")]), "server").expect("kebab loads");
    let snake: ServerConfig =
        load_group(&cfg(&[("BORED__SERVER__HTTP_PORT", "9100")]), "server").expect("snake loads");

    assert_eq!(kebab.http_port, snake.http_port);
    assert_eq!(kebab.http_port, 9100);
}

// ---------------------------------------------------------------------------
// secret redaction
// ---------------------------------------------------------------------------

/// `ConfigError::Load` forwards `config`'s own message, which embeds the offending
/// value on a type mismatch (`invalid type: string "…", expected an integer`). What
/// keeps a secret out of that message is that every secret leaf is `String`-typed,
/// and `String` cannot fail coercion.
///
/// That invariant is what this test pins: re-typing a secret leaf as anything
/// richer would put its value straight into the startup log.
#[test]
fn secret_leaves_never_leak_their_value_in_errors() {
    const SENTINEL: &str = "s3cr3t-sentinel-value-not-a-number";

    // oidc/client-secret: a value that would break any richer type still loads,
    // proving the leaf is String-typed and so cannot produce a coercion error...
    let config = cfg(&with(
        oidc_env(),
        &[("BORED__OIDC__CLIENT-SECRET", SENTINEL)],
    ));
    let oidc: OidcConfig = load_group(&config, "oidc").expect("a String secret accepts any value");
    assert_eq!(oidc.client_secret, SENTINEL);

    // ...and when a *different* field in the same group fails to coerce, the
    // resulting error must not carry the secret alongside it. `oidc` has no
    // non-String leaves to break coercion on, so this exercises `server`, which
    // has no secrets of its own — instead it pins the `Debug` output directly.
    let debug = format!("{oidc:?}");
    assert!(
        !debug.contains(SENTINEL),
        "OidcConfig's Debug impl leaked client_secret: {debug}"
    );

    // session/cookie-key: unlike client-secret, this leaf has real structural
    // validation (must decode to exactly 64 bytes), and the sentinel isn't
    // valid base64 — so it takes the `Invalid` path instead. That error must
    // still omit the value.
    let config = cfg(&[("BORED__SESSION__COOKIE-KEY", SENTINEL)]);
    let error = load_group::<SessionConfig>(&config, "session")
        .expect_err("non-base64 cookie key should be rejected");
    assert!(
        !error.to_string().contains(SENTINEL),
        "session/cookie-key leaked into an Invalid error: {error}"
    );

    // A structurally *valid* key still gets Debug-redacted.
    let session: SessionConfig = load_group(&cfg(&session_env()), "session").expect("should load");
    let debug = format!("{session:?}");
    assert!(
        !debug.contains(VALID_COOKIE_KEY),
        "SessionConfig's Debug impl leaked cookie_key: {debug}"
    );
}

/// The redaction guarantee that *is* unconditional: checks I perform myself never
/// echo the value, only the field path.
#[test]
fn validate_errors_name_the_field_but_never_the_value() {
    const SENTINEL: &str = "s3cr3t-sentinel-value";

    let config = cfg(&with(oidc_env(), &[("BORED__OIDC__REQUIRED-SCOPE", "   ")]));
    let error = load_group::<OidcConfig>(&config, "oidc").expect_err("blank scope rejected");
    assert!(error.to_string().contains("oidc.required-scope"));
    assert!(!error.to_string().contains("   "));

    let config = cfg(&with(oidc_env(), &[("BORED__OIDC__CLIENT-SECRET", " ")]));
    let error = load_group::<OidcConfig>(&config, "oidc").expect_err("blank secret rejected");
    assert_eq!(
        error.to_string(),
        "invalid config `oidc.client-secret`: must not be empty"
    );
    assert!(!error.to_string().contains(SENTINEL));
}

// ---------------------------------------------------------------------------
// layering
// ---------------------------------------------------------------------------

#[test]
fn env_layer_overrides_the_defaults_layer_on_the_same_kebab_key() {
    // Same leaf, supplied by both layers: the env override must win, which only
    // holds if both layers produce the identical kebab-case key.
    let config = cfg(&[
        ("BORED__OBSERVABILITY__ENVIRONMENT", "dev"),
        ("BORED__OBSERVABILITY__SERVICE-NAME", "overridden"),
    ]);
    let observability: ObservabilityConfig = load_group(&config, "observability").expect("loads");

    assert_eq!(observability.service_name, "overridden");
}

#[test]
#[serial]
fn sovereign_source_is_disabled_without_an_access_url() {
    // Guards the local-dev / e2e path: no access URL in this test process, so the
    // sovereign layer must be skipped and config must come from defaults + env.
    // SAFETY: #[serial] on this test excludes other threads from mutating env vars.
    unsafe { std::env::remove_var("SOVEREIGN_CONFIG_ACCESS_URL_FILE") };
    // SAFETY: #[serial] on this test excludes other threads from mutating env vars.
    unsafe { std::env::remove_var("SOVEREIGN_CONFIG_ACCESS_URL") };
    assert!(!sovereign_source_enabled());
}

#[test]
fn sovereign_layer_supplies_leaf_with_no_env_override() {
    let env = [("BORED__OBSERVABILITY__ENVIRONMENT", "dev")];

    let defaults_only: ObservabilityConfig =
        load_group(&cfg(&env), "observability").expect("loads");
    let with_sovereign: ObservabilityConfig = load_group(
        &cfg_with_sovereign(&[("observability.service-name", "from-sovereign")], &env),
        "observability",
    )
    .expect("loads");

    assert_eq!(
        with_sovereign.service_name, "from-sovereign",
        "the sovereign leaf must beat the built-in default"
    );
    assert_ne!(
        defaults_only.service_name, with_sovereign.service_name,
        "the fixture must differ from the default, or this proves nothing"
    );
}

/// The env layer is still the per-deploy escape hatch above sovereign-config —
/// nothing about supporting sovereign-config should quietly remove it.
#[test]
fn env_layer_still_overrides_the_sovereign_leaf() {
    let config = cfg_with_sovereign(
        &[("observability.service-name", "from-sovereign")],
        &[
            ("BORED__OBSERVABILITY__ENVIRONMENT", "dev"),
            ("BORED__OBSERVABILITY__SERVICE_NAME", "from-env"),
        ],
    );
    let observability: ObservabilityConfig = load_group(&config, "observability").expect("loads");

    assert_eq!(observability.service_name, "from-env");
}

// ---------------------------------------------------------------------------
// APP_VERSION override
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn app_version_override_falls_back_to_none_when_unset_or_blank() {
    // SAFETY: #[serial] on this test excludes other threads from mutating env vars.
    unsafe { std::env::remove_var("APP_VERSION") };
    assert_eq!(app_version_override(), None);

    // SAFETY: #[serial] on this test excludes other threads from mutating env vars.
    unsafe { std::env::set_var("APP_VERSION", "") };
    assert_eq!(app_version_override(), None);

    // SAFETY: #[serial] on this test excludes other threads from mutating env vars.
    unsafe { std::env::set_var("APP_VERSION", "1.2.3") };
    assert_eq!(app_version_override(), Some("1.2.3".to_string()));

    // SAFETY: #[serial] on this test excludes other threads from mutating env vars.
    unsafe { std::env::remove_var("APP_VERSION") };
}

//! Runtime configuration loaded from layered sources.
//!
//! Config is assembled from three layers (lowest priority first):
//!   1. in-memory defaults (ports, log level, database path, environment);
//!   2. the managed [`sovereign-config`] subtree — added **only** when the access
//!      URL is present (`SOVEREIGN_CONFIG_ACCESS_URL_FILE` / `SOVEREIGN_CONFIG_ACCESS_URL`),
//!      so local dev, unit tests and e2e (which have no sovereign-config server)
//!      fall back to plain env;
//!   3. environment overrides under the `BORED__` prefix with a `__` nesting separator.
//!
//! Each top-level group is its own self-contained DTO deserialized from its own
//! sub-branch (`oidc`, `session`, `observability`, `server`) — there is no umbrella
//! config struct. Every leaf in sovereign-config is text; rich field types (`u16`,
//! `Option<String>`, …) fold presence + coercion checks into deserialization, and each DTO
//! additionally implements [`ValidatedConfig`] for the residual checks the type
//! system can't express. All groups are loaded through the single [`load_group`]
//! choke point, which validates and redacts uniformly.
//!
//! `oidc` is the one **optional** group: bored deliberately supports an
//! auth-disabled mode for local hacking, so [`load_optional_oidc`] probes
//! `oidc.issuer-url` first and returns `None` when it's absent or blank, rather
//! than failing startup. When present, every other `oidc` leaf becomes required.

use base64::Engine;
use config::{Config, ConfigBuilder, Environment};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use sovereign_config_provider::SovereignConfigSource;

/// Env vars whose presence enables the sovereign-config source layer.
const SOVEREIGN_ACCESS_URL_FILE: &str = "SOVEREIGN_CONFIG_ACCESS_URL_FILE";
const SOVEREIGN_ACCESS_URL: &str = "SOVEREIGN_CONFIG_ACCESS_URL";

/// Prefix + separator for the environment override layer.
///
/// `BORED__OIDC__CLIENT-ID=…` maps to the `oidc.client-id` leaf. Multi-word leaf
/// segments are kebab-case to match sovereign-config path segments (which forbid
/// `_`), so the same key overrides across every layer.
const ENV_PREFIX: &str = "BORED";
const ENV_SEPARATOR: &str = "__";

/// A configuration failure, carrying enough to locate the offending group/field
/// but **never** the offending value (secret leaves stay redacted).
pub enum ConfigError {
    /// The layered `config::Config` could not be assembled (e.g. sovereign-config
    /// connection/reveal failed, or a default was rejected).
    Build(config::ConfigError),
    /// A group could not be deserialized from its sub-branch (missing field or a
    /// leaf that would not coerce to the target type).
    ///
    /// **Invariant:** `source` is `config`'s own message, which embeds the
    /// offending value on a type mismatch (`invalid type: string "…"`). Secret
    /// leaves must therefore stay `String`-typed — `String` cannot fail coercion,
    /// so a secret can never reach this branch and never appears in a startup log.
    /// Pinned by `secret_leaves_never_leak_their_value_in_errors`.
    Load {
        group: String,
        source: config::ConfigError,
    },
    /// A group deserialized but failed a [`ValidatedConfig::validate`] check.
    Invalid { path: String, reason: String },
}

impl ConfigError {
    /// Build an [`Invalid`](ConfigError::Invalid) error. `path` should name the
    /// field/leaf; `reason` must not embed any secret value.
    pub fn invalid(path: impl Into<String>, reason: impl Into<String>) -> Self {
        ConfigError::Invalid {
            path: path.into(),
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Build(source) => write!(f, "failed to build configuration: {source}"),
            ConfigError::Load { group, source } => {
                write!(f, "failed to load config group `{group}`: {source}")
            }
            ConfigError::Invalid { path, reason } => {
                write!(f, "invalid config `{path}`: {reason}")
            }
        }
    }
}

/// Startup failures surface through `Termination`, which prints `Debug`. Delegate to
/// `Display` so an operator sees the redacted one-line reason, not a struct dump.
impl std::fmt::Debug for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Build(source) | ConfigError::Load { source, .. } => Some(source),
            ConfigError::Invalid { .. } => None,
        }
    }
}

/// A config group DTO that can validate the residue its field types can't express.
///
/// The body is intentionally allowed to be empty when rich field types already
/// make illegal states unrepresentable.
pub trait ValidatedConfig: DeserializeOwned {
    fn validate(&self) -> Result<(), ConfigError>;
}

/// Returns whether the sovereign-config access URL is present in the environment.
///
/// When absent (local dev / e2e / unit tests) the sovereign source is skipped and
/// config comes from defaults + env only.
pub fn sovereign_source_enabled() -> bool {
    [SOVEREIGN_ACCESS_URL_FILE, SOVEREIGN_ACCESS_URL]
        .iter()
        .any(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty()))
}

/// `APP_VERSION` is a plain runtime override, deliberately kept outside the
/// layered `BORED__*` config (it's a burned-in-image / ad-hoc-test knob, not an
/// operator-managed setting). Unset in normal deploys, where the release tag
/// burned into the image at build time (`shared::app_version()`) is authoritative.
pub fn app_version_override() -> Option<String> {
    std::env::var("APP_VERSION").ok().filter(|v| !v.is_empty())
}

/// Assemble the layered `config::Config`.
///
/// This **blocks** while the sovereign-config source connects and reveals secrets
/// (on its own dedicated I/O thread), so call it once at startup, before any other
/// task is scheduled.
pub fn build_config() -> Result<Config, ConfigError> {
    let mut builder = Config::builder();
    builder = apply_defaults(builder)?;

    // Both sources are trimmed: whitespace is never meaningful in a config leaf and
    // is silently destructive in at least one place (see `Trimmed`).
    if sovereign_source_enabled() {
        builder = builder.add_source(Trimmed(
            SovereignConfigSource::initialise_from_default_environment(),
        ));
    }

    builder = builder.add_source(Trimmed(env_source()));

    builder.build().map_err(ConfigError::Build)
}

/// The environment override layer. `BORED__OIDC__CLIENT-ID` → `oidc.client-id`.
///
/// Also accepts the snake_case spelling (`BORED__OIDC__CLIENT_ID`), because `-` is
/// not legal in a POSIX shell variable name — `BORED__A__B-C=x` is parsed as a
/// command, not an assignment, so the kebab form is unusable from a plain shell.
fn env_source() -> KebabCaseEnvironment {
    KebabCaseEnvironment(
        Environment::with_prefix(ENV_PREFIX)
            .prefix_separator(ENV_SEPARATOR)
            .separator(ENV_SEPARATOR),
    )
}

/// Wraps any [`config::Source`], trimming surrounding whitespace from every string
/// leaf it produces.
///
/// Stray whitespace in a config value is always an accident — a copy-paste, or a
/// trailing newline from whatever wrote the leaf — and it can be silently
/// destructive. `oidc/required-scope` with a trailing space passes every startup
/// check (`require_non_empty` trims only for its emptiness test, then stores the
/// original), and then never matches a token scope, because JWT validation splits
/// the token's scope on whitespace and compares whole values. The result is 403 for
/// every user while startup, the deploy health gate and CI all report green.
///
/// Trimming centrally, at the point values enter the config, means no individual
/// field can reintroduce that — including fields added later. Applied to secrets
/// too: a credential with leading or trailing whitespace is far more likely to be a
/// paste artefact than intentional.
#[derive(Debug, Clone)]
struct Trimmed<S>(S);

impl<S> config::Source for Trimmed<S>
where
    S: config::Source + Clone + Send + Sync + 'static,
{
    fn clone_into_box(&self) -> Box<dyn config::Source + Send + Sync> {
        Box::new(self.clone())
    }

    fn collect(&self) -> Result<config::Map<String, config::Value>, config::ConfigError> {
        let mut collected = self.0.collect()?;
        for value in collected.values_mut() {
            trim_value(value);
        }
        Ok(collected)
    }
}

fn trim_value(value: &mut config::Value) {
    match &mut value.kind {
        config::ValueKind::String(text) => {
            let trimmed = text.trim();
            if trimmed.len() != text.len() {
                *text = trimmed.to_owned();
            }
        }
        config::ValueKind::Table(table) => table.values_mut().for_each(trim_value),
        config::ValueKind::Array(items) => items.iter_mut().for_each(trim_value),
        _ => {}
    }
}

/// Wraps [`Environment`], folding snake_case leaf names onto the canonical
/// kebab-case keys **before** the layers merge.
///
/// Aliasing at the serde level is not sufficient: `oidc.client-id` (from the
/// sovereign/defaults layer) and `oidc.client_id` (from env) are distinct keys, so
/// they do not override one another — serde sees the field twice and fails with
/// `duplicate field`. Normalising here means both spellings address the same key.
#[derive(Debug, Clone)]
struct KebabCaseEnvironment(Environment);

impl KebabCaseEnvironment {
    /// Read from an explicit map instead of the process environment (tests).
    #[cfg(test)]
    fn source(self, source: Option<config::Map<String, String>>) -> Self {
        KebabCaseEnvironment(self.0.source(source))
    }
}

impl config::Source for KebabCaseEnvironment {
    fn clone_into_box(&self) -> Box<dyn config::Source + Send + Sync> {
        Box::new(self.clone())
    }

    fn collect(&self) -> Result<config::Map<String, config::Value>, config::ConfigError> {
        Ok(self
            .0
            .collect()?
            .into_iter()
            // `config` has already lowercased the name and turned the `__`
            // separator into `.`, so any `_` still present is inside a leaf
            // segment. Config path segments never legitimately contain `_`
            // (sovereign-config forbids it), so this rewrite is unambiguous.
            .map(|(key, value)| (key.replace('_', "-"), value))
            .collect())
    }
}

fn apply_defaults(
    builder: ConfigBuilder<config::builder::DefaultState>,
) -> Result<ConfigBuilder<config::builder::DefaultState>, ConfigError> {
    // Every leaf is text; typed coercion happens at group deserialization.
    // No `oidc.*` / `session.*` defaults — both are absent unless configured,
    // which is what makes `oidc` an optional group (see `load_optional_oidc`).
    let defaults = [
        ("observability.environment", "dev"),
        ("observability.log-level", "info"),
        // Plain-HTTP fallback port; TLS (when configured) always binds :443.
        ("server.http-port", "3000"),
        ("server.static-dir", "./dist"),
        ("server.database-path", "/data/bored.db"),
    ];
    let mut builder = builder;
    for (key, value) in defaults {
        builder = builder
            .set_default(key, value)
            .map_err(ConfigError::Build)?;
    }
    Ok(builder)
}

/// The single choke point: deserialize a group from its sub-branch (presence +
/// coercion) then validate it, wrapping errors with the group path.
pub fn load_group<T: ValidatedConfig>(cfg: &Config, group: &str) -> Result<T, ConfigError> {
    let dto: T = cfg.get(group).map_err(|source| ConfigError::Load {
        group: group.to_string(),
        source,
    })?;
    dto.validate()?;
    Ok(dto)
}

/// Load the optional `oidc` group.
///
/// Probes `oidc.issuer-url` directly first: absent or blank ⇒ `Ok(None)` — bored's
/// auth-disabled mode for local hacking without a live IdP. A present, non-blank
/// issuer falls through to the normal [`load_group`] choke point, so every other
/// `oidc` leaf becomes a hard, named, fail-closed startup error if missing.
pub fn load_optional_oidc(cfg: &Config) -> Result<Option<OidcConfig>, ConfigError> {
    let issuer_present = match cfg.get::<String>("oidc.issuer-url") {
        Ok(value) => !value.trim().is_empty(),
        Err(config::ConfigError::NotFound(_)) => false,
        Err(source) => {
            return Err(ConfigError::Load {
                group: "oidc".to_string(),
                source,
            });
        }
    };
    if !issuer_present {
        return Ok(None);
    }
    load_group::<OidcConfig>(cfg, "oidc").map(Some)
}

/// Deserialize an optional leaf, treating a blank value as absent.
///
/// Deployment tooling routinely renders an unset value as an empty string, and
/// collapsing blank → `None` keeps that from being a hard startup failure and
/// preserves the pre-migration env semantics (e.g. `${OIDC_ISSUER_URL:-}`).
fn blank_as_none<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match Option::<String>::deserialize(deserializer)? {
        Some(raw) if !raw.trim().is_empty() => raw
            .trim()
            .parse()
            .map(Some)
            .map_err(serde::de::Error::custom),
        _ => Ok(None),
    }
}

/// Reject a secret/scalar that is empty after trimming, naming the field (never the value).
fn require_non_empty(path: &str, value: &str) -> Result<(), ConfigError> {
    if value.trim().is_empty() {
        return Err(ConfigError::invalid(path, "must not be empty"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// oidc
// ---------------------------------------------------------------------------

/// Optional, non-secret MCP service-account provider leaves. Present → the JWT
/// validator additionally accepts tokens issued by this second Authentik
/// application (see `backend/src/auth.rs::validate_jwt`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct OidcMcpConfig {
    #[serde(default, deserialize_with = "blank_as_none")]
    pub issuer_url: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub client_id: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct OidcConfig {
    pub issuer_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    pub required_scope: String,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub end_session_url: Option<String>,
    #[serde(default)]
    pub mcp: OidcMcpConfig,
}

/// Manual `Debug` so accidental `{:?}` formatting cannot leak `client_secret` —
/// mirrors `auth::AuthConfig`'s redacting `Debug` impl.
impl std::fmt::Debug for OidcConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcConfig")
            .field("issuer_url", &self.issuer_url)
            .field("client_id", &self.client_id)
            .field("client_secret", &"[REDACTED]")
            .field("redirect_uri", &self.redirect_uri)
            .field("required_scope", &self.required_scope)
            .field("end_session_url", &self.end_session_url)
            .field("mcp", &self.mcp)
            .finish()
    }
}

impl ValidatedConfig for OidcConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        require_non_empty("oidc.issuer-url", &self.issuer_url)?;
        require_non_empty("oidc.client-id", &self.client_id)?;
        require_non_empty("oidc.client-secret", &self.client_secret)?;
        require_non_empty("oidc.redirect-uri", &self.redirect_uri)?;
        require_non_empty("oidc.required-scope", &self.required_scope)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// session
// ---------------------------------------------------------------------------

/// Required whenever `oidc` is enabled — see `load_optional_oidc`.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SessionConfig {
    pub cookie_key: String,
}

/// Manual `Debug` so accidental `{:?}` formatting cannot leak `cookie_key`.
impl std::fmt::Debug for SessionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionConfig")
            .field("cookie_key", &"[REDACTED]")
            .finish()
    }
}

impl ValidatedConfig for SessionConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        require_non_empty("session.cookie-key", &self.cookie_key)?;
        // The cookie crate needs exactly 64 bytes of key material (independent
        // signing + encryption keys). Checked here, not just in
        // `AuthSessionManager::from_config`, so a malformed key is a fail-closed
        // startup error rather than a panic deep in request handling.
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&self.cookie_key)
            .map_err(|_| {
                ConfigError::invalid("session.cookie-key", "must be valid standard base64")
            })?;
        if decoded.len() != 64 {
            return Err(ConfigError::invalid(
                "session.cookie-key",
                format!("must decode to exactly 64 bytes (got {})", decoded.len()),
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// observability
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ObservabilityConfig {
    /// Which deployment this process *is*: `dev` or `prod`.
    ///
    /// The same value is set on the container as the
    /// `observability.deployment.environment` Docker label, which the homelab's
    /// Alloy turns into the `deployment_environment` Loki label — so the two
    /// come from one `APP_ENV` at deploy time and cannot drift. Deliberately
    /// *not* the branch name it used to hold: see `branch` below, and card #412.
    ///
    /// Left a plain `String` rather than an enum because non-deployed runs use
    /// other values — e2e passes `test`, and the default below is `dev`.
    pub environment: String,
    pub log_level: String,
    /// Branch this deployment was built from, on dev only — `None` in prod and
    /// for a local run. Reported by `/api/info` so the board can watermark a dev
    /// deployment with the branch it is serving; it is deliberately kept out of
    /// `environment` because a per-branch value there would start a new Loki
    /// stream set on every push.
    #[serde(default, deserialize_with = "blank_as_none")]
    pub branch: Option<String>,
}

impl ValidatedConfig for ObservabilityConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        require_non_empty("observability.environment", &self.environment)?;
        require_non_empty("observability.log-level", &self.log_level)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// server
// ---------------------------------------------------------------------------

/// How the process exposes itself. These are image-internal (identical across
/// deployments), so they come from config defaults or `BORED__SERVER__*` overrides
/// set by the image — **never put a `server/*` leaf in sovereign-config.**
///
/// That is a convention, not an enforced boundary: the sovereign layer is merged
/// wholesale, so a `server/*` leaf would be picked up like any other. Deliberately
/// unenforced: no `server` branch exists in either env subtree, and write access to
/// it already implies control of `oidc/client-secret` and `session/cookie-key`, so
/// this is a tidiness boundary rather than a security one.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ServerConfig {
    /// Plain-HTTP listen port, used only when TLS is not configured.
    pub http_port: u16,
    /// PEM certificate path; when set together with `tls-key`, the server binds
    /// TLS on `:443` instead of plain HTTP.
    #[serde(default, deserialize_with = "blank_as_none")]
    pub tls_cert: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub tls_key: Option<String>,
    /// Directory the compiled WASM frontend is served from.
    pub static_dir: String,
    pub database_path: String,
}

impl ServerConfig {
    /// The cert/key pair when both are configured, else `None` (plain HTTP).
    ///
    /// `validate` guarantees the pair is all-or-nothing, so a lone value never
    /// reaches here.
    pub fn tls_pair(&self) -> Option<(&str, &str)> {
        self.tls_cert.as_deref().zip(self.tls_key.as_deref())
    }
}

impl ValidatedConfig for ServerConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        require_non_empty("server.static-dir", &self.static_dir)?;
        require_non_empty("server.database-path", &self.database_path)?;
        if self.http_port == 0 {
            return Err(ConfigError::invalid("server.http-port", "must not be 0"));
        }
        match (&self.tls_cert, &self.tls_key) {
            (Some(_), None) => Err(ConfigError::invalid(
                "server.tls-key",
                "required when `server.tls-cert` is set",
            )),
            (None, Some(_)) => Err(ConfigError::invalid(
                "server.tls-cert",
                "required when `server.tls-key` is set",
            )),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests;

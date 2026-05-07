//! Pure helpers used by the audit history drawer in the frontend.
//!
//! These are intentionally pure (no JS / DOM access, no `chrono`) so they
//! can run as plain native Rust under `cargo test -p shared --lib`, which
//! is the test step CI runs in `.woodpecker/build.yml`. The frontend
//! supplies the runtime context (current time, local UTC offset, tz label,
//! current user's display name) and the helpers turn that plus an
//! `AuditLogEntry` into the user-facing strings.
//!
//! Three independent concerns, three modules:
//!
//! - [`actor`] — friendly actor labels («You» / «Someone» / display name /
//!   «Earlier collaborator»).
//! - [`time`] — local-timezone "Today / Yesterday / weekday / older"
//!   formatting + tooltip with explicit tz suffix.
//! - [`summary`] — one-line headline + optional sub-line per audit row,
//!   following the "primary label reflects the world *after* the change"
//!   rule from card #132.

pub mod actor;
pub mod summary;
pub mod time;

pub use actor::{label_actor, EARLIER_COLLABORATOR, SOMEONE, YOU};
pub use summary::{derive_summary, Summary};
pub use time::{format_history_time, format_history_tooltip, strip_surreal_wrapper};

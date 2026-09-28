//! The one error type every fallible handler returns.
//!
//! Before this module the routes returned `Result<_, StatusCode>`, which meant
//! every database call ended in `.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)`:
//! the underscore *is* the bug — it drops the only description of what failed,
//! so a 500 in production arrived with no log line saying why. `ApiError` keeps
//! the cause in the error value and logs it once, at the point where the error
//! becomes a response, which is also the point where `?` can finally be used.
//!
//! Three rules shape the design:
//!
//! 1. **Responses do not change.** A client error renders byte-for-byte as it
//!    did when handlers returned a bare `StatusCode` (empty body) or an
//!    `(StatusCode, &str)` tuple (plain text). That is why the client variants
//!    carry `Option<&'static str>` instead of always carrying a message.
//! 2. **An internal error is logged exactly once**, in `into_response`, rather
//!    than at each of the ~120 sites that used to discard it.
//! 3. **What that log line may say is a decision, made here** (card #366).
//!    The cause is reduced to an [`ErrorType`] and a redacted description at
//!    the moment it becomes an `ApiError::Internal` — never later, and never
//!    per call site. The raw driver message is not kept at all, because
//!    SurrealDB quotes the offending *value* back (a card body, a tag, a link
//!    reason) and a parse error quotes the *query*. See `crate::redact` for
//!    the policy itself.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use surrealdb::error::Db;

use crate::redact;

/// The name of the boards' unique index, as SurrealDB writes it into the
/// rejection message. The driver offers nothing structured to match on — no
/// error code, no index field — so this is a substring test on the message.
/// Keeping it here means there is exactly one such test in the crate instead
/// of one per site.
const BOARD_NAME_UNIQUE: &str = "board_name_unique";

/// As above, for the unique `(predecessor, successor)` index on `card_links`.
const CARD_LINKS_PAIR: &str = "card_links_pair";

/// The body the link routes return for a duplicate link. Lives here because
/// both the classifier below and `routes::links` (which checks for duplicates
/// before writing) have to produce the same response.
pub(crate) const ALREADY_LINKED_MESSAGE: &str = "these cards are already linked";

/// The `error.type` of a failed request: a bounded, typed classification.
///
/// The `observability` skill (§4) requires errors to be *typed, not stringly*:
/// anything used as a metric label or span attribute must come from an
/// enumeration, so its value set is fixed in code and cannot grow with user
/// input. This is that enumeration for the backend. Today it is logged as the
/// `error.type` field of the `request failed` line; #415 uses the same type as
/// the server span's `error.type` attribute and as a metric label.
///
/// Adding a variant fails to compile until [`ErrorType::as_str`] gives it a
/// label, because that `match` has no wildcard arm; and the test that walks
/// every variant checks each label is well-formed and distinct.
///
/// `EnumIter` (from `strum`) implements `strum::IntoEnumIterator`, whose
/// `ErrorType::iter()` yields each variant once, in declaration order — the
/// iterable variant list metric setup needs to pre-register one series per
/// label value (bring the trait into scope with `use strum::IntoEnumIterator`).
/// `EnumCount` adds the constant `ErrorType::COUNT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::EnumIter, strum::EnumCount)]
pub enum ErrorType {
    /// 404 — the board, column, card or link in the path does not exist.
    NotFound,
    /// 409 — a unique constraint says this would duplicate something.
    Conflict,
    /// 422 — well-formed request, but the values are not acceptable.
    Unprocessable,
    /// 500 — the database rejected a write on a schema constraint (a unique
    /// index, a field's type or `ASSERT`, a record that already exists) that
    /// bored does not translate into a client error.
    DbConstraint,
    /// 500 — any other failure inside the embedded database engine: a query
    /// that does not parse, a transaction conflict, a storage fault.
    DbEngine,
    /// 500 — the database driver's client layer (`surrealdb::Error::Api`):
    /// turning a result into our types, or talking to the engine at all.
    DbClient,
    /// 500 — `serde_json` failed on JSON we produced or read back ourselves.
    Serialization,
    /// 500 — a condition the code treats as impossible, described in words at
    /// the call site (`ApiError::internal("create returned no card row")`).
    Invariant,
}

impl ErrorType {
    /// The label value — what appears as `error.type` in a log line, and later
    /// on a span or metric. `const fn` so it can be evaluated at compile time;
    /// the return type is `&'static str` because every label is a literal
    /// baked into the binary, never built at run time.
    pub const fn as_str(self) -> &'static str {
        // Deliberately no `_ =>` arm: a new variant must be given a label here
        // before the crate compiles again.
        match self {
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::Unprocessable => "unprocessable",
            Self::DbConstraint => "db_constraint",
            Self::DbEngine => "db_engine",
            Self::DbClient => "db_client",
            Self::Serialization => "serialization",
            Self::Invariant => "invariant",
        }
    }
}

/// The part of a 500 that is allowed to reach a log line: its [`ErrorType`]
/// and a description that has already been through the redaction policy.
///
/// There is deliberately no way to build one from an arbitrary `String` or
/// `dyn Error`. The constructors are the `From` impls and
/// [`ApiError::internal`] below, and each of them decides what its source may
/// say. A future call site that wants to wrap a new kind of error has to add
/// a constructor here — i.e. has to make the same decision.
#[derive(Debug)]
pub(crate) struct InternalError {
    error_type: ErrorType,
    /// Human-readable, already redacted. A `String` because the database arm
    /// assembles it at run time from identifiers; the invariant arm stores a
    /// literal.
    detail: String,
}

/// Everything a handler can fail with.
///
/// The client variants take an optional message because the two halves of this
/// API answer differently: most routes reply with a bare status, while the link
/// routes spell out which of their four distinct 422s the caller hit. `None`
/// means "no body", `Some(m)` means "plain-text body `m`".
#[derive(Debug)]
pub(crate) enum ApiError {
    /// 404 — the board, column, card or link in the path does not exist.
    NotFound,
    /// 409 — a unique constraint says this would duplicate something.
    Conflict(Option<&'static str>),
    /// 422 — well-formed request, but the values are not acceptable.
    Unprocessable(Option<&'static str>),
    /// 500 — our fault. The cause is carried in its redacted form, and logged
    /// when this becomes a response.
    Internal(InternalError),
}

impl ApiError {
    /// 409 with no body — the shape the board routes have always returned.
    ///
    /// An associated `const` (rather than a function) so it can be used both as
    /// a value, `Err(ApiError::CONFLICT)`, and to build other constants.
    pub(crate) const CONFLICT: Self = Self::Conflict(None);

    /// 422 with no body, as the board, column, card and audit routes return.
    pub(crate) const UNPROCESSABLE: Self = Self::Unprocessable(None);

    /// A 500 for a condition the code treats as impossible.
    ///
    /// Takes only a `&'static str` — a literal written in our own source — so
    /// nothing computed at run time (and so nothing a user sent) can ride
    /// along into the log. An error *value* has its own `From` impl instead,
    /// which knows how to redact it.
    pub(crate) fn internal(message: &'static str) -> Self {
        Self::Internal(InternalError {
            error_type: ErrorType::Invariant,
            detail: message.to_string(),
        })
    }

    /// The typed classification of this error — the `error.type` value. Every
    /// variant has one, client errors included, so #415 can put it on every
    /// failed request's span rather than only on the 500s.
    pub(crate) fn error_type(&self) -> ErrorType {
        match self {
            Self::NotFound => ErrorType::NotFound,
            Self::Conflict(_) => ErrorType::Conflict,
            Self::Unprocessable(_) => ErrorType::Unprocessable,
            Self::Internal(internal) => internal.error_type,
        }
    }
}

/// Decide what a database error means to the caller.
///
/// This is the single place that inspects SurrealDB error text. A violated
/// unique index is the client's problem (409); anything else — a broken query,
/// an unreachable store, a deserialisation mismatch — is ours (500).
///
/// Classifying centrally, rather than at the two call sites that used to do it,
/// means a unique-index violation reported from anywhere gets the same answer
/// the dedicated call site gave. Only the boards and card_links tables have
/// unique indexes, so in practice the reachable behaviour is unchanged.
///
/// The match is on the index name *in its own position* in the driver's
/// message — ``Database index `x` already contains 'y'`` — never on the bare
/// name. The same message quotes the offending value, and that value is user
/// text: a card body or link reason containing `card_links_pair` would
/// otherwise be able to turn a genuine server fault into a 409, which is a
/// status this module deliberately does not log.
fn classify(error: surrealdb::Error) -> ApiError {
    // `to_string()` borrows the error, so it can still be used afterwards.
    let text = error.to_string();

    if text.contains(&index_violation(BOARD_NAME_UNIQUE)) {
        ApiError::CONFLICT
    } else if text.contains(&index_violation(CARD_LINKS_PAIR)) {
        ApiError::Conflict(Some(ALREADY_LINKED_MESSAGE))
    } else {
        // `text` — the verbatim message — is dropped here, unlogged. Only the
        // redacted description built from the error's structure survives.
        ApiError::Internal(describe_database_error(&error))
    }
}

/// The opening of SurrealDB's unique-index rejection for one index. Everything
/// after this prefix in the driver's message is the value that was rejected.
fn index_violation(index: &str) -> String {
    format!("Database index `{index}`")
}

/// Reduce a SurrealDB error to what the redaction policy lets through: its
/// variant path (`Db::FieldCheck`), plus — for the constraint variants, which
/// are the ones an operator most needs to place — the index or field name and
/// the `table:id` of the record, read from the error's *fields*, not its text.
///
/// The outer `match` is exhaustive over the driver's two error kinds (`Db`,
/// the embedded engine; `Api`, the client layer), so a third kind added by a
/// driver upgrade fails to compile here. The inner engine enum has ~250
/// variants and is `#[non_exhaustive]`, so it is matched only for the four
/// that carry a record; every other engine error is reported by variant name
/// alone.
fn describe_database_error(error: &surrealdb::Error) -> InternalError {
    // The variant path is read from `Debug`, which *does* contain values —
    // but `variant_path` stops before the first of them. The full rendering
    // lives only in this local and is dropped at the end of the function.
    let variant = redact::variant_path(&format!("{error:?}"));

    let (error_type, detail) = match error {
        surrealdb::Error::Db(db) => match db {
            // `{ thing, index, .. }` binds two fields by name and ignores the
            // rest — including `value`, the user's text, which is never read.
            Db::IndexExists { thing, index, .. } => (
                ErrorType::DbConstraint,
                format!(
                    "{variant} index={} record={}",
                    redact::identifier(index),
                    redact::thing(thing)
                ),
            ),
            Db::RecordExists { thing } => (
                ErrorType::DbConstraint,
                format!("{variant} record={}", redact::thing(thing)),
            ),
            // `|` in a pattern matches either variant; both bind the same
            // names with the same types, so one arm handles both. `value` and
            // `check` are skipped: `check` for `FieldValue` is our ASSERT
            // clause, which may quote literals from the schema.
            Db::FieldCheck { thing, field, .. } | Db::FieldValue { thing, field, .. } => (
                ErrorType::DbConstraint,
                format!(
                    "{variant} field={} record={}",
                    redact::identifier(&field.to_string()),
                    redact::record_string(thing)
                ),
            ),
            _ => (ErrorType::DbEngine, variant),
        },
        surrealdb::Error::Api(_) => (ErrorType::DbClient, variant),
    };
    InternalError { error_type, detail }
}

/// Lets `?` turn a database error into an `ApiError` with no closure at the
/// call site: this impl is what the `?` operator calls on the way out.
impl From<surrealdb::Error> for ApiError {
    fn from(error: surrealdb::Error) -> Self {
        classify(error)
    }
}

/// JSON that we produced or read back ourselves. A client's malformed JSON
/// never reaches a handler — axum's `Json` extractor rejects it first — so a
/// `serde_json` failure here is always an internal inconsistency.
///
/// `serde_json`'s message quotes the value it choked on (``invalid type:
/// string "a card body", expected u32``), so only its category and position
/// are kept. `classify()` is serde_json's own coarse kind — `Io`, `Syntax`,
/// `Data` or `Eof` — and says which of the four went wrong.
impl From<serde_json::Error> for ApiError {
    fn from(error: serde_json::Error) -> Self {
        Self::Internal(InternalError {
            error_type: ErrorType::Serialization,
            detail: format!(
                "serde_json {:?} error at line {} column {}",
                error.classify(),
                error.line(),
                error.column()
            ),
        })
    }
}

/// How the error reaches the wire. axum calls this for the `Err` side of any
/// handler returning `Result<_, ApiError>`.
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // Read before `match self` moves `self` apart. Only the internal arm
        // logs it today; #415 records it on the request span for every arm.
        let error_type = self.error_type();
        match self {
            Self::NotFound => StatusCode::NOT_FOUND.into_response(),
            Self::Conflict(message) => client_error(StatusCode::CONFLICT, message),
            Self::Unprocessable(message) => client_error(StatusCode::UNPROCESSABLE_ENTITY, message),
            Self::Internal(internal) => {
                // The one log line the old `map_err(|_| …)` never wrote. It is
                // emitted here, and only here, so a cause cannot be logged
                // twice on its way up through nested helpers. The enclosing
                // tower-http trace span supplies the method and path.
                //
                // DECISION (card #366): this line carries `error.type` and a
                // *redacted* description — never the driver's verbatim
                // message, which can quote user content and query text. The
                // redaction happened when `internal` was built; see
                // `crate::redact` for what is kept and why.
                //
                // `"error.type" = …` uses a string-literal field name because
                // `type` is a Rust keyword and cannot appear in tracing's
                // dotted-identifier field syntax.
                tracing::error!(
                    "error.type" = error_type.as_str(),
                    error = %internal.detail,
                    "request failed"
                );
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
    }
}

/// Render a client error the way the handlers used to render it by hand: a
/// bare status (no body, no content type) when there is nothing to say, and
/// axum's `(status, &str)` plain-text response when there is.
fn client_error(status: StatusCode, message: Option<&'static str>) -> Response {
    match message {
        Some(message) => (status, message).into_response(),
        None => status.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use strum::{EnumCount, IntoEnumIterator};

    use super::*;

    /// Every variant has a label, found by *walking the enum* rather than by
    /// listing cases — so a variant added later is covered without editing
    /// this test. The labels must be distinct (a shared label would merge two
    /// series) and lowercase snake_case (the shape of a semconv value).
    #[test]
    fn every_error_type_has_a_distinct_snake_case_label() {
        let mut seen = HashSet::new();
        for error_type in ErrorType::iter() {
            let label = error_type.as_str();
            assert!(
                !label.is_empty() && label.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{error_type:?} has a malformed label {label:?}"
            );
            assert!(
                seen.insert(label),
                "{error_type:?} reuses the label {label:?}"
            );
        }
        // The iterator really did visit every variant: strum derives `COUNT`
        // from the enum definition independently of `iter()`.
        assert_eq!(seen.len(), ErrorType::COUNT);
    }

    /// `ApiError::error_type` agrees with the status each variant renders.
    #[test]
    fn client_errors_carry_their_own_error_type() {
        assert_eq!(ApiError::NotFound.error_type(), ErrorType::NotFound);
        assert_eq!(ApiError::CONFLICT.error_type(), ErrorType::Conflict);
        assert_eq!(
            ApiError::UNPROCESSABLE.error_type(),
            ErrorType::Unprocessable
        );
        assert_eq!(ApiError::internal("x").error_type(), ErrorType::Invariant);
    }
}

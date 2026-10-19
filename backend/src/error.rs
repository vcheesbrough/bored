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
//!    Postgres can quote the offending *value* back (`invalid input syntax for
//!    type integer: "…"`, and a unique violation's detail names the duplicate
//!    key). Only the SQLSTATE code and the constraint and table *names* are
//!    kept. See `crate::redact` for the policy itself.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};

use crate::redact;

/// The name of the boards' unique constraint (`backend/migrations/`). Postgres
/// reports it as a structured field of the error, so matching it is an exact
/// comparison, never a search through message text.
const BOARD_NAME_UNIQUE: &str = "board_name_unique";

/// As above, for the unique `(predecessor_id, successor_id)` constraint on
/// `card_links`.
const CARD_LINKS_PAIR: &str = "card_links_pair";

/// SQLSTATE `23505`, `unique_violation`.
const UNIQUE_VIOLATION: &str = "23505";

/// SQLSTATE `23503`, `foreign_key_violation`.
const FOREIGN_KEY_VIOLATION: &str = "23503";

/// The foreign key from a card to its column.
const CARDS_COLUMN_FKEY: &str = "cards_column_id_fkey";

/// For a write that *puts a card into a column* (create, move, update): a
/// violation of the card → column foreign key means the column vanished
/// between the route's check and the write — a create racing the column's
/// delete — and is the caller's 404, exactly what the route answers when it
/// sees the column missing up front. Anything else classifies as usual.
///
/// Applied at those call sites only, not in [`classify`]: the same constraint
/// also fires when a *column delete* finds cards still in it, which is an
/// expected retry (`routes::cascade`), not a missing column.
pub(crate) fn card_column_write_error(error: sqlx::Error) -> ApiError {
    if let sqlx::Error::Database(database) = &error
        && database.code().as_deref() == Some(FOREIGN_KEY_VIOLATION)
        && database.constraint() == Some(CARDS_COLUMN_FKEY)
    {
        return ApiError::NotFound;
    }
    classify(error)
}

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
    /// 500 — Postgres rejected a write on an integrity constraint (SQLSTATE
    /// class `23`: a unique or foreign key, a `NOT NULL`, a `CHECK`) that
    /// bored does not translate into a client error.
    DbConstraint,
    /// 500 — any other error Postgres itself reported: a statement that does
    /// not parse, a serialization failure, a full disk.
    DbEngine,
    /// 500 — the driver's own side (`sqlx`): turning a row into our types, a
    /// column that is missing, a protocol mismatch.
    DbClient,
    /// 500 — the database could not be reached at all: no free connection
    /// within the pool's timeout, a closed pool, or a broken socket.
    DbUnavailable,
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
            Self::DbUnavailable => "db_unavailable",
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
/// This is the single place that inspects database errors. A violated unique
/// constraint the API knows about is the client's problem (409); anything
/// else — a broken query, an unreachable server, a decoding mismatch — is ours
/// (500).
///
/// Classifying centrally, rather than at the two call sites that used to do it,
/// means a unique violation reported from anywhere gets the same answer the
/// dedicated call site gave. Only the boards and card_links tables have
/// constraints the API translates, so in practice the reachable behaviour is
/// unchanged.
///
/// The match is on the SQLSTATE code and the constraint *name*, both
/// structured fields of the server's error — never on its message, which can
/// quote user text.
fn classify(error: sqlx::Error) -> ApiError {
    match unique_index_conflict(&error) {
        Some(conflict) => conflict,
        // The verbatim message is never kept. Only the redacted description
        // built from the error's structure survives.
        None => ApiError::Internal(describe_database_error(&error)),
    }
}

/// The client-facing conflict a unique violation stands for, if `error` is one
/// of the two this API translates; `None` for anything else.
fn unique_index_conflict(error: &sqlx::Error) -> Option<ApiError> {
    let sqlx::Error::Database(database) = error else {
        return None;
    };
    match (database.code().as_deref(), database.constraint()) {
        (Some(UNIQUE_VIOLATION), Some(BOARD_NAME_UNIQUE)) => Some(ApiError::CONFLICT),
        (Some(UNIQUE_VIOLATION), Some(CARD_LINKS_PAIR)) => {
            Some(ApiError::Conflict(Some(ALREADY_LINKED_MESSAGE)))
        }
        _ => None,
    }
}

/// The [`ErrorType`] a database error would become as a response — without
/// consuming it. The database span (`db.rs`) labels itself and the
/// `bored.db.errors` metric with this, so a failed call and the request it
/// fails carry the same class, from one classification rather than two.
pub(crate) fn database_error_type(error: &sqlx::Error) -> ErrorType {
    match unique_index_conflict(error) {
        Some(conflict) => conflict.error_type(),
        None => describe_database_error(error).error_type,
    }
}

/// The redacted one-line description of a database error — the same text a
/// 500's log line carries (`postgres sqlstate=23505 constraint=… table=…`).
/// For callers outside request handling, such as the SurrealDB import.
pub(crate) fn database_error_description(error: &sqlx::Error) -> String {
    describe_database_error(error).detail
}

/// Reduce a database error to what the redaction policy lets through.
///
/// For an error Postgres reported: its SQLSTATE code (five characters from a
/// fixed table, e.g. `23503`), plus the constraint and table *names* when the
/// server supplies them — the fields an operator needs to place the failure.
/// The message and the `detail` field are never read: they quote values
/// (`Key (name)=(…) already exists`).
///
/// For a driver-side error: the variant's name only. `sqlx::Error` is
/// `#[non_exhaustive]`, so the wildcard arm classes anything a later sqlx
/// adds as a client fault, named by `redact::variant_path`.
fn describe_database_error(error: &sqlx::Error) -> InternalError {
    let (error_type, detail) = match error {
        sqlx::Error::Database(database) => {
            let code = database.code();
            let code = code.as_deref().unwrap_or("unknown");
            // Class `23` is "integrity constraint violation".
            let error_type = if code.starts_with("23") {
                ErrorType::DbConstraint
            } else {
                ErrorType::DbEngine
            };
            let mut detail = format!("postgres sqlstate={}", redact::identifier(code));
            if let Some(constraint) = database.constraint() {
                detail.push_str(&format!(" constraint={}", redact::identifier(constraint)));
            }
            if let Some(table) = database.table() {
                detail.push_str(&format!(" table={}", redact::identifier(table)));
            }
            (error_type, detail)
        }
        sqlx::Error::PoolTimedOut => (ErrorType::DbUnavailable, "sqlx PoolTimedOut".to_string()),
        sqlx::Error::PoolClosed => (ErrorType::DbUnavailable, "sqlx PoolClosed".to_string()),
        sqlx::Error::Io(io) => (
            ErrorType::DbUnavailable,
            // The kind is an enum (`ConnectionRefused`, `TimedOut`, …); the
            // OS message is left out with everything else free-form.
            format!("sqlx Io kind={:?}", io.kind()),
        ),
        sqlx::Error::Tls(_) => (ErrorType::DbUnavailable, "sqlx Tls".to_string()),
        // The column *name* is ours (it comes from the query), so it may be
        // kept; the decode error itself can quote the value.
        sqlx::Error::ColumnDecode { index, .. } => (
            ErrorType::DbClient,
            // sqlx renders a column named by string with its `Debug` quotes
            // (`"body"`); a positional one is a bare number.
            format!(
                "sqlx ColumnDecode column={}",
                redact::identifier(index.trim_matches('"'))
            ),
        ),
        sqlx::Error::ColumnNotFound(column) => (
            ErrorType::DbClient,
            format!("sqlx ColumnNotFound column={}", redact::identifier(column)),
        ),
        other => (
            ErrorType::DbClient,
            format!("sqlx {}", redact::variant_path(&format!("{other:?}"))),
        ),
    };
    InternalError { error_type, detail }
}

/// Lets `?` turn a database error into an `ApiError` with no closure at the
/// call site: this impl is what the `?` operator calls on the way out.
impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
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
        // logs it; every arm hands it to the response, below.
        let error_type = self.error_type();
        let mut response = match self {
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
        };
        // Carry the class to the transport layer: `server_span::record_response`
        // reads this extension to set `error.type` on the request's server
        // span and on `http.server.request.duration` (card #415). Extensions
        // are server-side only; nothing here reaches the wire.
        response.extensions_mut().insert(error_type);
        response
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

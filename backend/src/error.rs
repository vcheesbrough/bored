//! The one error type every fallible handler returns.
//!
//! Before this module the routes returned `Result<_, StatusCode>`, which meant
//! every database call ended in `.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)`:
//! the underscore *is* the bug — it drops the only description of what failed,
//! so a 500 in production arrived with no log line saying why. `ApiError` keeps
//! the cause in the error value and logs it once, at the point where the error
//! becomes a response, which is also the point where `?` can finally be used.
//!
//! Two rules shape the design:
//!
//! 1. **Responses do not change.** A client error renders byte-for-byte as it
//!    did when handlers returned a bare `StatusCode` (empty body) or an
//!    `(StatusCode, &str)` tuple (plain text). That is why the client variants
//!    carry `Option<&'static str>` instead of always carrying a message.
//! 2. **An internal error is logged exactly once**, in `into_response`, rather
//!    than at each of the ~120 sites that used to discard it.

use std::error::Error;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};

/// A boxed, type-erased cause. `dyn Error` is a *trait object*: it stands for
/// "some concrete error type, decided at run time". It has to live behind a
/// pointer (`Box`) because its size isn't known at compile time, and it needs
/// `Send + Sync + 'static` so the error can cross an `.await` inside an axum
/// handler and be owned by the response future.
type Source = Box<dyn Error + Send + Sync + 'static>;

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
    /// 500 — our fault. The cause is carried, not discarded, and logged when
    /// this becomes a response.
    Internal(Source),
}

impl ApiError {
    /// 409 with no body — the shape the board routes have always returned.
    ///
    /// An associated `const` (rather than a function) so it can be used both as
    /// a value, `Err(ApiError::CONFLICT)`, and to build other constants.
    pub(crate) const CONFLICT: Self = Self::Conflict(None);

    /// 422 with no body, as the board, column, card and audit routes return.
    pub(crate) const UNPROCESSABLE: Self = Self::Unprocessable(None);

    /// Wrap any cause as a 500.
    ///
    /// `impl Into<Source>` accepts anything the standard library can box into a
    /// `dyn Error`: a concrete error type, a `String`, or a `&'static str` for
    /// the handful of "this should be impossible" cases the routes describe in
    /// words rather than with an error value.
    pub(crate) fn internal(source: impl Into<Source>) -> Self {
        Self::Internal(source.into())
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
    // `to_string()` borrows the error, so it can still be moved afterwards.
    let text = error.to_string();

    if text.contains(&index_violation(BOARD_NAME_UNIQUE)) {
        ApiError::CONFLICT
    } else if text.contains(&index_violation(CARD_LINKS_PAIR)) {
        ApiError::Conflict(Some(ALREADY_LINKED_MESSAGE))
    } else {
        ApiError::internal(error)
    }
}

/// The opening of SurrealDB's unique-index rejection for one index. Everything
/// after this prefix in the driver's message is the value that was rejected.
fn index_violation(index: &str) -> String {
    format!("Database index `{index}`")
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
impl From<serde_json::Error> for ApiError {
    fn from(error: serde_json::Error) -> Self {
        Self::internal(error)
    }
}

/// How the error reaches the wire. axum calls this for the `Err` side of any
/// handler returning `Result<_, ApiError>`.
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND.into_response(),
            Self::Conflict(message) => client_error(StatusCode::CONFLICT, message),
            Self::Unprocessable(message) => client_error(StatusCode::UNPROCESSABLE_ENTITY, message),
            Self::Internal(source) => {
                // The one log line the old `map_err(|_| …)` never wrote. It is
                // emitted here, and only here, so a cause cannot be logged
                // twice on its way up through nested helpers. The enclosing
                // tower-http trace span supplies the method and path.
                tracing::error!(error = %source, "request failed");
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

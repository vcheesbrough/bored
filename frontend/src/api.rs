use gloo_net::http::{Request, Response};

use crate::telemetry::{self, ActiveSpan, SpanContext, SpanKind, otlp::keys};

/// Inspect a server response for 401 Unauthorized and redirect to the login
/// route if so. The redirect navigates the entire SPA away — when the user
/// returns from Authentik, the page reloads cleanly with a fresh session
/// cookie.
///
/// Every API call funnels through this so a single re-auth path covers the
/// whole frontend. Returns the response unchanged for non-401 status codes;
/// returns an error for 401 to short-circuit the caller.
fn check_auth(resp: Response) -> Result<Response, gloo_net::Error> {
    if resp.status() == 401 {
        let location = leptos::prelude::window().location();
        let path = location.pathname().unwrap_or_else(|_| "/".into());
        let search = location.search().unwrap_or_default();
        let hash = location.hash().unwrap_or_default();
        let return_to = format!("{path}{search}{hash}");
        let encoded = js_sys::encode_uri_component(&return_to)
            .as_string()
            .unwrap_or_default();
        // `set_href` triggers a top-level navigation; the SPA will tear down
        // and the browser will load the new URL. This is intentional: the
        // login route is server-side and any in-flight requests no longer
        // matter once the session is gone.
        let _ = location.set_href(&format!("/auth/login?return_to={encoded}"));
        return Err(gloo_net::Error::GlooError(
            "redirecting to /auth/login".into(),
        ));
    }
    Ok(resp)
}

/// What the user is told when a mutation is refused because the link to the
/// server is down. Callers surface it through whatever they already do with an
/// error, so the wording has to stand alone.
const OFFLINE_MESSAGE: &str = "disconnected from the server — changes are not being saved";

/// A failed API call: the underlying error (whose text goes to the console,
/// exactly as before card #416), plus what telemetry needs to file it — the
/// HTTP status when there was one, a fixed `error.type` class, and the context
/// of the request's own `http.client` span, so the call site's log line links
/// to the very request that failed.
#[derive(Debug)]
pub struct ApiError {
    error: gloo_net::Error,
    status: Option<u16>,
    error_type: &'static str,
    trace: Option<SpanContext>,
}

impl ApiError {
    /// An error that never reached the network — no span, no status.
    fn local(error: gloo_net::Error, error_type: &'static str) -> Self {
        Self {
            error,
            status: None,
            error_type,
            trace: None,
        }
    }
}

// `Display` delegates to the wrapped error, so every `"…: {e}"` console line
// reads exactly as it did when these functions returned `gloo_net::Error`.
impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl telemetry::Reportable for ApiError {
    fn error_type(&self) -> String {
        // A status beats the coarse class: `404` says more than `http`.
        self.status
            .filter(|status| *status >= 400)
            .map_or_else(|| self.error_type.to_string(), |status| status.to_string())
    }

    fn status(&self) -> Option<u16> {
        self.status
    }

    fn trace(&self) -> Option<SpanContext> {
        self.trace
    }
}

/// Refuse a mutating request while this tab is disconnected.
///
/// This is the single choke point for the rule that a disconnected UI performs
/// no mutations (see [`crate::connection`]): every mutating function below
/// starts with `offline_guard()?`, so there is no way to add a new mutation
/// that forgets it beyond forgetting the line itself. Reads are deliberately
/// left alone — refreshing a stale view is exactly what a reconnecting tab
/// wants to do.
///
/// It also matters that the request is never *sent*. A queued mutation that
/// lands after the server comes back would write the user's pre-outage
/// intention over whatever happened in the meantime.
fn offline_guard() -> Result<(), ApiError> {
    if crate::connection::is_connected() {
        Ok(())
    } else {
        Err(ApiError::local(
            gloo_net::Error::GlooError(OFFLINE_MESSAGE.to_string()),
            "offline",
        ))
    }
}

/// The same guard for the card-link routes, which report failures as a
/// [`LinkApiError`] so the link editor can show the server's own wording.
/// `status: 0` is this module's existing convention for "never reached the
/// server" (see the `From<ApiError>` impl below).
fn offline_guard_link() -> Result<(), LinkApiError> {
    if crate::connection::is_connected() {
        Ok(())
    } else {
        Err(LinkApiError {
            status: 0,
            message: OFFLINE_MESSAGE.to_string(),
            trace: None,
        })
    }
}

/// Split a route label such as `"GET /api/boards/{slug}"` into its method and
/// URL template. Both halves are slices of the `'static` label, so they can go
/// straight into span attributes without allocating.
fn split_route(route: &'static str) -> (&'static str, &'static str) {
    route.split_once(' ').unwrap_or(("", route))
}

/// Send one request inside its own `http.client` span and read the answer.
///
/// - `route` is the span name, `"<METHOD> <template>"` — a template, never the
///   concrete URL, so the name stays bounded however many cards there are.
/// - `parent` is the span this request belongs under: a screen's load span,
///   or `None` for a user action, which makes this request the root of its own
///   trace. Passed explicitly; see `crate::telemetry` for why.
/// - `read` turns the response into the caller's value (usually `.json()`).
///   It is also handed this request's span context, for a caller that folds a
///   refusal into its own error type (the link routes).
///
/// The request carries `traceparent` naming this span, which is what makes
/// Traefik's and the server's spans its children: one trace from the click
/// to the database.
async fn send<T, F, Fut>(
    route: &'static str,
    parent: Option<SpanContext>,
    request: Result<Request, gloo_net::Error>,
    read: F,
) -> Result<T, ApiError>
where
    // `FnOnce(Response) -> Fut` is "a closure called once with the response,
    // returning a future"; `Fut: Future<…>` names what that future yields.
    F: FnOnce(Response, Option<SpanContext>) -> Fut,
    Fut: std::future::Future<Output = Result<T, gloo_net::Error>>,
{
    let (method, template) = split_route(route);
    let mut span = telemetry::start_span(route, SpanKind::Client, parent);
    span.set_attribute(keys::HTTP_REQUEST_METHOD, method);
    span.set_attribute(keys::URL_TEMPLATE, template);

    // A closure that files an error against this span. It borrows `span`
    // mutably only while it runs, so the span stays usable in between.
    let fail = |span: &mut ActiveSpan,
                error: gloo_net::Error,
                status: Option<u16>,
                error_type: &'static str| {
        let class = status
            .filter(|status| *status >= 400)
            .map_or_else(|| error_type.to_string(), |status| status.to_string());
        span.fail(class);
        ApiError {
            error,
            status,
            error_type,
            trace: span.context(),
        }
    };

    let request = match request {
        Ok(request) => request,
        Err(error) => return Err(fail(&mut span, error, None, "encode")),
    };
    if let Some(traceparent) = span.traceparent() {
        // `headers()` is the request's live header list, so setting on it
        // changes what is sent.
        request.headers().set("traceparent", &traceparent);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => return Err(fail(&mut span, error, None, "network")),
    };
    let status = response.status();
    span.set_attribute(keys::HTTP_RESPONSE_STATUS_CODE, status);
    let response = match check_auth(response) {
        Ok(response) => response,
        Err(error) => return Err(fail(&mut span, error, Some(status), "http")),
    };
    if status >= 400 {
        // Marked now; `read` still runs, because some callers (the link
        // routes) read the error body for the user.
        span.fail(status.to_string());
    }
    match read(response, span.context()).await {
        Ok(value) => Ok(value),
        Err(error) => Err(fail(&mut span, error, Some(status), "decode")),
    }
    // `span` is dropped here, which ends and buffers it.
}

/// Read a body-less answer: `Ok` for 2xx, else an error naming the route.
async fn expect_ok(label: &'static str, response: Response) -> Result<(), gloo_net::Error> {
    if response.ok() {
        Ok(())
    } else {
        Err(gloo_net::Error::GlooError(format!(
            "{label}: server returned {}",
            response.status()
        )))
    }
}

pub async fn fetch_app_info() -> Result<shared::AppInfo, gloo_net::Error> {
    // `/api/info` is intentionally public, but go through `check_auth` anyway
    // so the redirect-on-401 invariant holds uniformly. Not traced: it is
    // polled, and a trace per poll would be noise about nothing.
    check_auth(Request::get("/api/info").send().await?)?
        .json::<shared::AppInfo>()
        .await
}

/// `/api/info` with a deadline, for the connection heartbeat.
///
/// A plain `fetch` has no timeout of its own. A server that accepts the
/// connection and then never answers — a wedged container, a proxy holding an
/// upstream socket open — would leave the heartbeat awaiting forever: no
/// failure is ever recorded, so the tab goes on believing it is connected and
/// goes on accepting edits. That is the one shape of outage a bare `await`
/// cannot see, so this request is aborted if it has not completed within
/// `timeout_ms`, and the abort surfaces as an ordinary `Err`.
///
/// Aborted rather than merely abandoned: racing the fetch against a timer would
/// stop *waiting* for it but leave the request open in the browser, and a tab
/// sat against a wedged server would pile up one of those per heartbeat.
///
/// Not traced, like [`fetch_app_info`]: a span every ten seconds per tab.
pub async fn fetch_app_info_within(timeout_ms: u32) -> Result<shared::AppInfo, gloo_net::Error> {
    // `ok()`: a browser too old to have `AbortController` still gets a
    // heartbeat, just one without a deadline — better than a heartbeat that
    // fails every time and pins the tab offline.
    let controller = web_sys::AbortController::new().ok();
    let signal = controller.as_ref().map(web_sys::AbortController::signal);

    // Dropping a `Timeout` cancels it, so holding this until the function
    // returns means the abort fires only if the request is still in flight at
    // the deadline — a request that finished in time takes its timer with it.
    let _deadline = controller.map(|controller| {
        gloo_timers::callback::Timeout::new(timeout_ms, move || controller.abort())
    });

    // The signal covers the body as well as the headers, so a response that
    // starts and then stalls mid-JSON is cut off by the same deadline.
    check_auth(
        Request::get("/api/info")
            .abort_signal(signal.as_ref())
            .send()
            .await?,
    )?
    .json::<shared::AppInfo>()
    .await
}

/// Fetch the current user's identity from `/api/me`.
/// Used by the navbar to render `preferred_username` + avatar.
pub async fn fetch_me() -> Result<shared::UserInfo, ApiError> {
    send(
        "GET /api/me",
        None,
        Request::get("/api/me").build(),
        |r, _| async move { r.json::<shared::UserInfo>().await },
    )
    .await
}

/// `parent`: the screen load this fetch belongs to, if any.
pub async fn fetch_boards(parent: Option<SpanContext>) -> Result<Vec<shared::Board>, ApiError> {
    send(
        "GET /api/boards",
        parent,
        Request::get("/api/boards").build(),
        |r, _| async move { r.json::<Vec<shared::Board>>().await },
    )
    .await
}

pub async fn create_board(name: String) -> Result<shared::Board, ApiError> {
    offline_guard()?;
    send(
        "POST /api/boards",
        None,
        Request::post("/api/boards").json(&shared::CreateBoardRequest { name }),
        |r, _| async move { r.json::<shared::Board>().await },
    )
    .await
}

pub async fn delete_board(board_id: &str) -> Result<(), ApiError> {
    offline_guard()?;
    send(
        "DELETE /api/boards/{slug}",
        None,
        Request::delete(&format!("/api/boards/{board_id}")).build(),
        |r, _| expect_ok("delete_board", r),
    )
    .await
}

pub async fn fetch_board(
    board_id: &str,
    parent: Option<SpanContext>,
) -> Result<shared::Board, ApiError> {
    send(
        "GET /api/boards/{slug}",
        parent,
        Request::get(&format!("/api/boards/{board_id}")).build(),
        |r, _| async move { r.json::<shared::Board>().await },
    )
    .await
}

pub async fn fetch_columns(
    board_id: &str,
    parent: Option<SpanContext>,
) -> Result<Vec<shared::Column>, ApiError> {
    send(
        "GET /api/boards/{slug}/columns",
        parent,
        Request::get(&format!("/api/boards/{board_id}/columns")).build(),
        |r, _| async move { r.json::<Vec<shared::Column>>().await },
    )
    .await
}

pub async fn create_column(
    board_id: &str,
    name: String,
    position: i32,
) -> Result<shared::Column, ApiError> {
    offline_guard()?;
    send(
        "POST /api/boards/{slug}/columns",
        None,
        Request::post(&format!("/api/boards/{board_id}/columns"))
            .json(&shared::CreateColumnRequest { name, position }),
        |r, _| async move { r.json::<shared::Column>().await },
    )
    .await
}

pub async fn update_column(
    column_id: &str,
    payload: shared::UpdateColumnRequest,
) -> Result<shared::Column, ApiError> {
    offline_guard()?;
    send(
        "PUT /api/columns/{id}",
        None,
        Request::put(&format!("/api/columns/{column_id}")).json(&payload),
        |r, _| async move { r.json::<shared::Column>().await },
    )
    .await
}

pub async fn delete_column(column_id: &str) -> Result<(), ApiError> {
    offline_guard()?;
    send(
        "DELETE /api/columns/{id}",
        None,
        Request::delete(&format!("/api/columns/{column_id}")).build(),
        |r, _| expect_ok("delete_column", r),
    )
    .await
}

/// Fetch a card by its human-readable sequential number via `GET /api/cards/by-number/:number`.
/// Used when the URL carries `?card=<number>` rather than the internal ULID.
pub async fn fetch_card_by_number(
    number: u32,
    parent: Option<SpanContext>,
) -> Result<shared::Card, ApiError> {
    send(
        "GET /api/cards/by-number/{number}",
        parent,
        Request::get(&format!("/api/cards/by-number/{number}")).build(),
        |r, _| async move { r.json::<shared::Card>().await },
    )
    .await
}

pub async fn fetch_cards(
    column_id: &str,
    parent: Option<SpanContext>,
) -> Result<Vec<shared::Card>, ApiError> {
    send(
        "GET /api/columns/{id}/cards",
        parent,
        Request::get(&format!("/api/columns/{column_id}/cards")).build(),
        |r, _| async move { r.json::<Vec<shared::Card>>().await },
    )
    .await
}

pub async fn create_card(column_id: &str, body: String) -> Result<shared::Card, ApiError> {
    offline_guard()?;
    send(
        "POST /api/columns/{id}/cards",
        None,
        Request::post(&format!("/api/columns/{column_id}/cards")).json(
            &shared::CreateCardRequest {
                body,
                // Cards are always born untagged; tags are added from the card
                // itself once it exists.
                tags: Vec::new(),
            },
        ),
        |r, _| async move { r.json::<shared::Card>().await },
    )
    .await
}

pub async fn update_card(
    card_id: &str,
    payload: shared::UpdateCardRequest,
) -> Result<shared::Card, ApiError> {
    offline_guard()?;
    send(
        "PUT /api/cards/{id}",
        None,
        Request::put(&format!("/api/cards/{card_id}")).json(&payload),
        |r, _| async move { r.json::<shared::Card>().await },
    )
    .await
}

pub async fn delete_card(card_id: &str) -> Result<(), ApiError> {
    offline_guard()?;
    send(
        "DELETE /api/cards/{id}",
        None,
        Request::delete(&format!("/api/cards/{card_id}")).build(),
        |r, _| expect_ok("delete_card", r),
    )
    .await
}

/// `PUT /api/boards/:slug/columns/reorder`
///
/// Sends the complete desired column order; the server reassigns every
/// `position` field and returns the updated sorted list. The caller should
/// apply the returned list to keep local state in sync.
pub async fn reorder_columns(
    board_slug: &str,
    order: Vec<String>,
) -> Result<Vec<shared::Column>, ApiError> {
    offline_guard()?;
    send(
        "PUT /api/boards/{slug}/columns/reorder",
        None,
        Request::put(&format!("/api/boards/{board_slug}/columns/reorder"))
            .json(&shared::ColumnsReorderRequest { order }),
        |r, _| async move { r.json::<Vec<shared::Column>>().await },
    )
    .await
}

/// `PUT /api/columns/:id/cards/reorder`
///
/// Sends the complete desired top-to-bottom order of one column's cards. The
/// server applies it and broadcasts a `CardMoved` for each card it actually had
/// to write, so the view updates over SSE like any other move; the returned
/// list is the authoritative order for callers that want it.
pub async fn reorder_cards(
    column_id: &str,
    order: Vec<String>,
) -> Result<Vec<shared::Card>, ApiError> {
    offline_guard()?;
    send(
        "PUT /api/columns/{id}/cards/reorder",
        None,
        Request::put(&format!("/api/columns/{column_id}/cards/reorder"))
            .json(&shared::CardsReorderRequest { order }),
        |r, _| async move { r.json::<Vec<shared::Card>>().await },
    )
    .await
}

pub async fn fetch_board_history(board_slug: &str) -> Result<Vec<shared::AuditLogEntry>, ApiError> {
    send(
        "GET /api/boards/{slug}/history",
        None,
        Request::get(&format!("/api/boards/{board_slug}/history")).build(),
        |r, _| async move { r.json::<Vec<shared::AuditLogEntry>>().await },
    )
    .await
}

pub async fn fetch_column_history(column_id: &str) -> Result<Vec<shared::AuditLogEntry>, ApiError> {
    send(
        "GET /api/columns/{id}/history",
        None,
        Request::get(&format!("/api/columns/{column_id}/history")).build(),
        |r, _| async move { r.json::<Vec<shared::AuditLogEntry>>().await },
    )
    .await
}

pub async fn fetch_card_history(card_id: &str) -> Result<Vec<shared::AuditLogEntry>, ApiError> {
    send(
        "GET /api/cards/{id}/history",
        None,
        Request::get(&format!("/api/cards/{card_id}/history")).build(),
        |r, _| async move { r.json::<Vec<shared::AuditLogEntry>>().await },
    )
    .await
}

pub async fn restore_audit_entry(audit_id: &str) -> Result<Vec<shared::AuditLogEntry>, ApiError> {
    offline_guard()?;
    send(
        "POST /api/audit/{id}/restore",
        None,
        Request::post(&format!("/api/audit/{audit_id}/restore")).build(),
        |r, _| async move { r.json::<Vec<shared::AuditLogEntry>>().await },
    )
    .await
}

pub async fn move_card(
    card_id: &str,
    column_id: String,
    position: i32,
) -> Result<shared::Card, ApiError> {
    offline_guard()?;
    send(
        "POST /api/cards/{id}/move",
        None,
        Request::post(&format!("/api/cards/{card_id}/move")).json(&shared::MoveCardRequest {
            column_id,
            position,
        }),
        |r, _| async move { r.json::<shared::Card>().await },
    )
    .await
}

// ── Card links ───────────────────────────────────────────────────────────

pub async fn fetch_board_links(
    board_slug: &str,
    parent: Option<SpanContext>,
) -> Result<Vec<shared::CardLink>, ApiError> {
    send(
        "GET /api/boards/{slug}/links",
        parent,
        Request::get(&format!("/api/boards/{board_slug}/links")).build(),
        |r, _| async move { r.json::<Vec<shared::CardLink>>().await },
    )
    .await
}

/// A failed link mutation. Unlike the other routes, the link endpoints answer
/// a refused request with a short plain-text explanation (four different
/// things are 422 for a link), and the editor shows that text to the user.
#[derive(Debug, Clone)]
pub struct LinkApiError {
    pub status: u16,
    pub message: String,
    /// The request's span, for the log line (card #416).
    trace: Option<SpanContext>,
}

impl std::fmt::Display for LinkApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.status)
    }
}

impl telemetry::Reportable for LinkApiError {
    fn error_type(&self) -> String {
        // `message` is the server's wording, which is shown to the user but
        // never exported; the class is the status, or "request" when the
        // request never got one.
        if self.status == 0 {
            "request".to_string()
        } else {
            self.status.to_string()
        }
    }

    fn status(&self) -> Option<u16> {
        (self.status != 0).then_some(self.status)
    }

    fn trace(&self) -> Option<SpanContext> {
        self.trace
    }
}

impl From<ApiError> for LinkApiError {
    fn from(e: ApiError) -> Self {
        Self {
            status: 0,
            message: format!("request failed: {e}"),
            trace: e.trace,
        }
    }
}

/// Read a link route's answer: `read_ok` on 2xx, else a [`LinkApiError`]
/// carrying the server's text (or a generic message when the body is empty —
/// 404, 500) and the request's span.
///
/// The inner `Result` is the link outcome; the outer one is the transport, as
/// [`send`] expects. Reading the refusal's body happens inside `send`, so the
/// span covers it.
async fn read_link<T, Fut>(
    response: Response,
    trace: Option<SpanContext>,
    read_ok: impl FnOnce(Response) -> Fut,
) -> Result<Result<T, LinkApiError>, gloo_net::Error>
where
    Fut: std::future::Future<Output = Result<T, gloo_net::Error>>,
{
    let status = response.status();
    if response.ok() {
        return read_ok(response).await.map(Ok);
    }
    let body = response.text().await.unwrap_or_default();
    let message = if body.trim().is_empty() {
        format!("server returned {status}")
    } else {
        body
    };
    Ok(Err(LinkApiError {
        status,
        message,
        trace,
    }))
}

/// Flatten `send`'s transport result around a link outcome.
fn flatten_link<T>(outcome: Result<Result<T, LinkApiError>, ApiError>) -> Result<T, LinkApiError> {
    // `?` on the outer `Result` converts an `ApiError` through the `From`
    // impl above; the inner one is already the link error.
    outcome?
}

pub async fn create_card_link(
    card_id: &str,
    payload: shared::CreateCardLinkRequest,
) -> Result<shared::CardLink, LinkApiError> {
    offline_guard_link()?;
    flatten_link(
        send(
            "POST /api/cards/{id}/links",
            None,
            Request::post(&format!("/api/cards/{card_id}/links")).json(&payload),
            |r, trace| {
                read_link(
                    r,
                    trace,
                    |r| async move { r.json::<shared::CardLink>().await },
                )
            },
        )
        .await,
    )
}

pub async fn update_card_link(
    link_id: &str,
    payload: shared::UpdateCardLinkRequest,
) -> Result<shared::CardLink, LinkApiError> {
    offline_guard_link()?;
    flatten_link(
        send(
            "PUT /api/links/{id}",
            None,
            Request::put(&format!("/api/links/{link_id}")).json(&payload),
            |r, trace| {
                read_link(
                    r,
                    trace,
                    |r| async move { r.json::<shared::CardLink>().await },
                )
            },
        )
        .await,
    )
}

pub async fn delete_card_link(link_id: &str) -> Result<(), LinkApiError> {
    offline_guard_link()?;
    flatten_link(
        send(
            "DELETE /api/links/{id}",
            None,
            Request::delete(&format!("/api/links/{link_id}")).build(),
            // A `DELETE` answers with no body: success reads nothing.
            |r, trace| read_link(r, trace, |_| async { Ok(()) }),
        )
        .await,
    )
}

#[cfg(test)]
mod tests {
    use super::split_route;

    #[test]
    fn route_labels_split_into_method_and_template() {
        assert_eq!(
            split_route("GET /api/boards/{slug}/columns"),
            ("GET", "/api/boards/{slug}/columns")
        );
        assert_eq!(
            split_route("DELETE /api/links/{id}"),
            ("DELETE", "/api/links/{id}")
        );
    }
}

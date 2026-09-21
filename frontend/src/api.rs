use gloo_net::http::{Request, Response};

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
fn offline_guard() -> Result<(), gloo_net::Error> {
    if crate::connection::is_connected() {
        Ok(())
    } else {
        Err(gloo_net::Error::GlooError(OFFLINE_MESSAGE.to_string()))
    }
}

/// The same guard for the card-link routes, which report failures as a
/// [`LinkApiError`] so the link editor can show the server's own wording.
/// `status: 0` is this module's existing convention for "never reached the
/// server" (see the `From<gloo_net::Error>` impl below).
fn offline_guard_link() -> Result<(), LinkApiError> {
    if crate::connection::is_connected() {
        Ok(())
    } else {
        Err(LinkApiError {
            status: 0,
            message: OFFLINE_MESSAGE.to_string(),
        })
    }
}

pub async fn fetch_app_info() -> Result<shared::AppInfo, gloo_net::Error> {
    // `/api/info` is intentionally public, but go through `check_auth` anyway
    // so the redirect-on-401 invariant holds uniformly.
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
pub async fn fetch_me() -> Result<shared::UserInfo, gloo_net::Error> {
    check_auth(Request::get("/api/me").send().await?)?
        .json::<shared::UserInfo>()
        .await
}

pub async fn fetch_boards() -> Result<Vec<shared::Board>, gloo_net::Error> {
    check_auth(Request::get("/api/boards").send().await?)?
        .json::<Vec<shared::Board>>()
        .await
}

pub async fn create_board(name: String) -> Result<shared::Board, gloo_net::Error> {
    offline_guard()?;
    check_auth(
        Request::post("/api/boards")
            .json(&shared::CreateBoardRequest { name })
            .expect("failed to serialize create board request")
            .send()
            .await?,
    )?
    .json::<shared::Board>()
    .await
}

pub async fn delete_board(board_id: &str) -> Result<(), gloo_net::Error> {
    offline_guard()?;
    let resp = check_auth(
        Request::delete(&format!("/api/boards/{board_id}"))
            .send()
            .await?,
    )?;
    if resp.ok() {
        Ok(())
    } else {
        Err(gloo_net::Error::GlooError(format!(
            "delete_board: server returned {}",
            resp.status()
        )))
    }
}

pub async fn fetch_board(board_id: &str) -> Result<shared::Board, gloo_net::Error> {
    check_auth(
        Request::get(&format!("/api/boards/{board_id}"))
            .send()
            .await?,
    )?
    .json::<shared::Board>()
    .await
}

pub async fn fetch_columns(board_id: &str) -> Result<Vec<shared::Column>, gloo_net::Error> {
    check_auth(
        Request::get(&format!("/api/boards/{board_id}/columns"))
            .send()
            .await?,
    )?
    .json::<Vec<shared::Column>>()
    .await
}

pub async fn create_column(
    board_id: &str,
    name: String,
    position: i32,
) -> Result<shared::Column, gloo_net::Error> {
    offline_guard()?;
    check_auth(
        Request::post(&format!("/api/boards/{board_id}/columns"))
            .json(&shared::CreateColumnRequest { name, position })
            .expect("failed to serialize create column request")
            .send()
            .await?,
    )?
    .json::<shared::Column>()
    .await
}

pub async fn update_column(
    column_id: &str,
    payload: shared::UpdateColumnRequest,
) -> Result<shared::Column, gloo_net::Error> {
    offline_guard()?;
    check_auth(
        Request::put(&format!("/api/columns/{column_id}"))
            .json(&payload)?
            .send()
            .await?,
    )?
    .json::<shared::Column>()
    .await
}

pub async fn delete_column(column_id: &str) -> Result<(), gloo_net::Error> {
    offline_guard()?;
    let resp = check_auth(
        Request::delete(&format!("/api/columns/{column_id}"))
            .send()
            .await?,
    )?;
    if resp.ok() {
        Ok(())
    } else {
        Err(gloo_net::Error::GlooError(format!(
            "delete_column: server returned {}",
            resp.status()
        )))
    }
}

/// Fetch a card by its human-readable sequential number via `GET /api/cards/by-number/:number`.
/// Used when the URL carries `?card=<number>` rather than the internal ULID.
pub async fn fetch_card_by_number(number: u32) -> Result<shared::Card, gloo_net::Error> {
    check_auth(
        Request::get(&format!("/api/cards/by-number/{number}"))
            .send()
            .await?,
    )?
    .json::<shared::Card>()
    .await
}

pub async fn fetch_cards(column_id: &str) -> Result<Vec<shared::Card>, gloo_net::Error> {
    check_auth(
        Request::get(&format!("/api/columns/{column_id}/cards"))
            .send()
            .await?,
    )?
    .json::<Vec<shared::Card>>()
    .await
}

pub async fn create_card(column_id: &str, body: String) -> Result<shared::Card, gloo_net::Error> {
    offline_guard()?;
    check_auth(
        Request::post(&format!("/api/columns/{column_id}/cards"))
            .json(&shared::CreateCardRequest {
                body,
                // Cards are always born untagged; tags are added from the card
                // itself once it exists.
                tags: Vec::new(),
            })?
            .send()
            .await?,
    )?
    .json::<shared::Card>()
    .await
}

pub async fn update_card(
    card_id: &str,
    payload: shared::UpdateCardRequest,
) -> Result<shared::Card, gloo_net::Error> {
    offline_guard()?;
    check_auth(
        Request::put(&format!("/api/cards/{card_id}"))
            .json(&payload)?
            .send()
            .await?,
    )?
    .json::<shared::Card>()
    .await
}

pub async fn delete_card(card_id: &str) -> Result<(), gloo_net::Error> {
    offline_guard()?;
    let resp = check_auth(
        Request::delete(&format!("/api/cards/{card_id}"))
            .send()
            .await?,
    )?;
    if resp.ok() {
        Ok(())
    } else {
        Err(gloo_net::Error::GlooError(format!(
            "delete_card: server returned {}",
            resp.status()
        )))
    }
}

/// `PUT /api/boards/:slug/columns/reorder`
///
/// Sends the complete desired column order; the server reassigns every
/// `position` field and returns the updated sorted list. The caller should
/// apply the returned list to keep local state in sync.
pub async fn reorder_columns(
    board_slug: &str,
    order: Vec<String>,
) -> Result<Vec<shared::Column>, gloo_net::Error> {
    offline_guard()?;
    check_auth(
        Request::put(&format!("/api/boards/{board_slug}/columns/reorder"))
            .json(&shared::ColumnsReorderRequest { order })?
            .send()
            .await?,
    )?
    .json::<Vec<shared::Column>>()
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
) -> Result<Vec<shared::Card>, gloo_net::Error> {
    offline_guard()?;
    check_auth(
        Request::put(&format!("/api/columns/{column_id}/cards/reorder"))
            .json(&shared::CardsReorderRequest { order })?
            .send()
            .await?,
    )?
    .json::<Vec<shared::Card>>()
    .await
}

pub async fn fetch_board_history(
    board_slug: &str,
) -> Result<Vec<shared::AuditLogEntry>, gloo_net::Error> {
    check_auth(
        Request::get(&format!("/api/boards/{board_slug}/history"))
            .send()
            .await?,
    )?
    .json::<Vec<shared::AuditLogEntry>>()
    .await
}

pub async fn fetch_column_history(
    column_id: &str,
) -> Result<Vec<shared::AuditLogEntry>, gloo_net::Error> {
    check_auth(
        Request::get(&format!("/api/columns/{column_id}/history"))
            .send()
            .await?,
    )?
    .json::<Vec<shared::AuditLogEntry>>()
    .await
}

pub async fn fetch_card_history(
    card_id: &str,
) -> Result<Vec<shared::AuditLogEntry>, gloo_net::Error> {
    check_auth(
        Request::get(&format!("/api/cards/{card_id}/history"))
            .send()
            .await?,
    )?
    .json::<Vec<shared::AuditLogEntry>>()
    .await
}

pub async fn restore_audit_entry(
    audit_id: &str,
) -> Result<Vec<shared::AuditLogEntry>, gloo_net::Error> {
    offline_guard()?;
    check_auth(
        Request::post(&format!("/api/audit/{audit_id}/restore"))
            .send()
            .await?,
    )?
    .json::<Vec<shared::AuditLogEntry>>()
    .await
}

pub async fn move_card(
    card_id: &str,
    column_id: String,
    position: i32,
) -> Result<shared::Card, gloo_net::Error> {
    offline_guard()?;
    check_auth(
        Request::post(&format!("/api/cards/{card_id}/move"))
            .json(&shared::MoveCardRequest {
                column_id,
                position,
            })?
            .send()
            .await?,
    )?
    .json::<shared::Card>()
    .await
}

// ── Card links ───────────────────────────────────────────────────────────

pub async fn fetch_board_links(board_slug: &str) -> Result<Vec<shared::CardLink>, gloo_net::Error> {
    check_auth(
        Request::get(&format!("/api/boards/{board_slug}/links"))
            .send()
            .await?,
    )?
    .json::<Vec<shared::CardLink>>()
    .await
}

/// A failed link mutation. Unlike the other routes, the link endpoints answer
/// a refused request with a short plain-text explanation (four different
/// things are 422 for a link), and the editor shows that text to the user.
#[derive(Debug, Clone)]
pub struct LinkApiError {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for LinkApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.status)
    }
}

impl From<gloo_net::Error> for LinkApiError {
    fn from(e: gloo_net::Error) -> Self {
        Self {
            status: 0,
            message: format!("request failed: {e}"),
        }
    }
}

/// Turn a non-2xx link response into a [`LinkApiError`] carrying the server's
/// text, or a generic message when the body is empty (404, 500).
async fn link_error(resp: Response) -> LinkApiError {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let message = if body.trim().is_empty() {
        format!("server returned {status}")
    } else {
        body
    };
    LinkApiError { status, message }
}

pub async fn create_card_link(
    card_id: &str,
    payload: shared::CreateCardLinkRequest,
) -> Result<shared::CardLink, LinkApiError> {
    offline_guard_link()?;
    let resp = check_auth(
        Request::post(&format!("/api/cards/{card_id}/links"))
            .json(&payload)?
            .send()
            .await?,
    )?;
    if !resp.ok() {
        return Err(link_error(resp).await);
    }
    Ok(resp.json::<shared::CardLink>().await?)
}

pub async fn update_card_link(
    link_id: &str,
    payload: shared::UpdateCardLinkRequest,
) -> Result<shared::CardLink, LinkApiError> {
    offline_guard_link()?;
    let resp = check_auth(
        Request::put(&format!("/api/links/{link_id}"))
            .json(&payload)?
            .send()
            .await?,
    )?;
    if !resp.ok() {
        return Err(link_error(resp).await);
    }
    Ok(resp.json::<shared::CardLink>().await?)
}

pub async fn delete_card_link(link_id: &str) -> Result<(), LinkApiError> {
    offline_guard_link()?;
    let resp = check_auth(
        Request::delete(&format!("/api/links/{link_id}"))
            .send()
            .await?,
    )?;
    if !resp.ok() {
        return Err(link_error(resp).await);
    }
    Ok(())
}

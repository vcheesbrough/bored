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
        // `set_href` triggers a top-level navigation; the SPA will tear down
        // and the browser will load the new URL. This is intentional: the
        // login route is server-side and any in-flight requests no longer
        // matter once the session is gone.
        let _ = leptos::prelude::window().location().set_href("/auth/login");
        return Err(gloo_net::Error::GlooError(
            "redirecting to /auth/login".into(),
        ));
    }
    Ok(resp)
}

pub async fn fetch_app_info() -> Result<shared::AppInfo, gloo_net::Error> {
    // `/api/info` is intentionally public, but go through `check_auth` anyway
    // so the redirect-on-401 invariant holds uniformly.
    check_auth(Request::get("/api/info").send().await?)?
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

pub async fn fetch_card(card_id: &str) -> Result<shared::Card, gloo_net::Error> {
    check_auth(
        Request::get(&format!("/api/cards/{card_id}"))
            .send()
            .await?,
    )?
    .json::<shared::Card>()
    .await
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
    check_auth(
        Request::post(&format!("/api/columns/{column_id}/cards"))
            .json(&shared::CreateCardRequest { body })?
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

/// `PUT /api/boards/:id/columns/reorder`
///
/// Sends the complete desired column order; the server reassigns every
/// `position` field and returns the updated sorted list. The caller should
/// apply the returned list to keep local state in sync.
pub async fn reorder_columns(
    board_id: &str,
    order: Vec<String>,
) -> Result<Vec<shared::Column>, gloo_net::Error> {
    check_auth(
        Request::put(&format!("/api/boards/{board_id}/columns/reorder"))
            .json(&shared::ColumnsReorderRequest { order })?
            .send()
            .await?,
    )?
    .json::<Vec<shared::Column>>()
    .await
}

pub async fn move_card(
    card_id: &str,
    column_id: String,
    position: i32,
) -> Result<shared::Card, gloo_net::Error> {
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

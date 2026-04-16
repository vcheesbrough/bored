use gloo_net::http::Request;

pub async fn fetch_boards() -> Result<Vec<shared::Board>, gloo_net::Error> {
    Request::get("/api/boards")
        .send()
        .await?
        .json::<Vec<shared::Board>>()
        .await
}

pub async fn create_board(name: String) -> Result<shared::Board, gloo_net::Error> {
    Request::post("/api/boards")
        .json(&shared::CreateBoardRequest { name })
        .expect("failed to serialize create board request")
        .send()
        .await?
        .json::<shared::Board>()
        .await
}

pub async fn fetch_columns(board_id: &str) -> Result<Vec<shared::Column>, gloo_net::Error> {
    Request::get(&format!("/api/boards/{board_id}/columns"))
        .send()
        .await?
        .json::<Vec<shared::Column>>()
        .await
}

pub async fn create_column(
    board_id: &str,
    name: String,
    position: i32,
) -> Result<shared::Column, gloo_net::Error> {
    Request::post(&format!("/api/boards/{board_id}/columns"))
        .json(&shared::CreateColumnRequest { name, position })
        .expect("failed to serialize create column request")
        .send()
        .await?
        .json::<shared::Column>()
        .await
}

pub async fn fetch_cards(column_id: &str) -> Result<Vec<shared::Card>, gloo_net::Error> {
    Request::get(&format!("/api/columns/{column_id}/cards"))
        .send()
        .await?
        .json::<Vec<shared::Card>>()
        .await
}

pub async fn create_card(
    column_id: &str,
    title: String,
    description: Option<String>,
) -> Result<shared::Card, gloo_net::Error> {
    Request::post(&format!("/api/columns/{column_id}/cards"))
        .json(&shared::CreateCardRequest { title, description })
        .expect("failed to serialize create card request")
        .send()
        .await?
        .json::<shared::Card>()
        .await
}

pub async fn update_card(
    card_id: &str,
    payload: shared::UpdateCardRequest,
) -> Result<shared::Card, gloo_net::Error> {
    Request::put(&format!("/api/cards/{card_id}"))
        .json(&payload)
        .expect("failed to serialize update card request")
        .send()
        .await?
        .json::<shared::Card>()
        .await
}

pub async fn delete_card(card_id: &str) -> Result<(), gloo_net::Error> {
    Request::delete(&format!("/api/cards/{card_id}"))
        .send()
        .await?;
    Ok(())
}

pub async fn move_card(
    card_id: &str,
    column_id: String,
    position: i32,
) -> Result<shared::Card, gloo_net::Error> {
    Request::post(&format!("/api/cards/{card_id}/move"))
        .json(&shared::MoveCardRequest {
            column_id,
            position,
        })
        .expect("failed to serialize move card request")
        .send()
        .await?
        .json::<shared::Card>()
        .await
}

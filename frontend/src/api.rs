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

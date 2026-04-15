use gloo_net::http::Request;

pub async fn fetch_boards() -> Vec<shared::Board> {
    Request::get("/api/boards")
        .send()
        .await
        .expect("failed to fetch boards")
        .json::<Vec<shared::Board>>()
        .await
        .expect("failed to parse boards")
}

pub async fn create_board(name: String) -> shared::Board {
    Request::post("/api/boards")
        .json(&shared::CreateBoardRequest { name })
        .expect("failed to serialize create board request")
        .send()
        .await
        .expect("failed to create board")
        .json::<shared::Board>()
        .await
        .expect("failed to parse board")
}

pub async fn fetch_columns(board_id: &str) -> Vec<shared::Column> {
    Request::get(&format!("/api/boards/{board_id}/columns"))
        .send()
        .await
        .expect("failed to fetch columns")
        .json::<Vec<shared::Column>>()
        .await
        .expect("failed to parse columns")
}

pub async fn create_column(
    board_id: &str,
    name: String,
    position: i32,
) -> shared::Column {
    Request::post(&format!("/api/boards/{board_id}/columns"))
        .json(&shared::CreateColumnRequest { name, position })
        .expect("failed to serialize create column request")
        .send()
        .await
        .expect("failed to create column")
        .json::<shared::Column>()
        .await
        .expect("failed to parse column")
}

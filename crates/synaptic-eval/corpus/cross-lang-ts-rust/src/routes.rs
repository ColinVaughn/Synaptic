// Rust (axum) backend. Registers POST /session (handled by create_session) and
// GET /items (handled by list_items). The TS client only calls /session, so
// list_items is a distractor the cross-language layer must NOT couple to it.

pub fn create_session() -> u32 {
    42
}

pub fn list_items() -> u32 {
    7
}

pub fn list_users() -> u32 {
    3
}

pub fn app() {
    let api = Router::new().route("/users", get(list_users));
    Router::new()
        .route("/session", post(create_session))
        .route("/items", get(list_items))
        .nest("/api", api);
}

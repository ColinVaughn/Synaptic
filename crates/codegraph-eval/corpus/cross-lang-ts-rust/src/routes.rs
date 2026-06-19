// Rust (axum) backend: registers a POST /session route handled by create_session.

pub fn create_session() -> u32 {
    42
}

pub fn app() {
    Router::new().route("/session", post(create_session));
}

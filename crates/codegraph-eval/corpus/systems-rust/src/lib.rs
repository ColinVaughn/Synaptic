mod router;

pub fn validate(path: &str) -> bool {
    !path.is_empty()
}

pub fn handle_request(path: &str) -> u32 {
    if validate(path) {
        router::route(path)
    } else {
        0
    }
}

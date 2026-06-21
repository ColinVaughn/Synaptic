//! HTTP transport for the MCP server (C3b) + a small REST surface (C3d).
//!
//! Streamable-HTTP over `/mcp`:
//!   - `POST` — one JSON-RPC request → its JSON response (a notification → 202).
//!     Stateful by default: an `initialize` mints an `Mcp-Session-Id` (returned
//!     as a response header); later requests carry it (unknown ⇒ 404 ⇒ the
//!     client re-initializes). A missing id on a non-initialize request is
//!     tolerated, so simple request/response clients keep working.
//!   - `GET` (`Accept: text/event-stream`) — opens a keep-alive SSE stream (the
//!     server-initiated channel; we have no pushes yet, so it's a heartbeat).
//!   - `DELETE` — terminates a session.
//!
//! On every `/mcp` request after initialization, a present-but-unsupported
//! `MCP-Protocol-Version` header is rejected with 400 (per the 2025-11-25
//! transport); an absent header is tolerated (assume `2025-03-26`), and the
//! `initialize` request is exempt (its version comes from negotiation).
//!
//! An idle reaper drops sessions after [`DEFAULT_SESSION_IDLE`]. This realizes
//! the MCP Streamable-HTTP transport (see [`crate::session`]).
//!
//! A read-only **REST** surface (`/api/*`, C3d) wraps the same engine calls the
//! MCP tools use, for non-MCP clients / a future web explorer.
//!
//! Shared security (both surfaces): a **constant-time API-key check** (`X-API-Key`
//! or `Authorization: Bearer`, scheme case-insensitive; blank key disables auth)
//! and a **DNS-rebinding Host allowlist** for specific/loopback binds.

use std::collections::HashSet;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::session::{SessionStore, DEFAULT_SESSION_IDLE};
use crate::Server;

/// Acquire the engine read lock, recovering from poisoning. A poisoned lock left
/// valid data behind (the writer panicked); one panic must not wedge every later
/// request, so we recover the guard instead of cascading the panic.
fn read_server(s: &RwLock<Server>) -> RwLockReadGuard<'_, Server> {
    s.read().unwrap_or_else(|e| e.into_inner())
}

/// Acquire the engine write lock, recovering from poisoning (see [`read_server`]).
fn write_server(s: &RwLock<Server>) -> RwLockWriteGuard<'_, Server> {
    s.write().unwrap_or_else(|e| e.into_inner())
}

/// 500 for when a `spawn_blocking` worker panicked (its `JoinError`) — return a
/// response instead of propagating the panic into the handler.
fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
}

#[derive(Clone)]
struct HttpState {
    /// Shared graph engine. `RwLock` (not `Mutex`) so read-only requests run
    /// concurrently; a rare hot-reload takes the write lock. All request work is
    /// dispatched off the async executor via `spawn_blocking` so a slow PR tool
    /// (blocking `gh`/`git`) never stalls the runtime or other requests (C1).
    server: Arc<RwLock<Server>>,
    /// Required key, or `None` when auth is disabled.
    api_key: Option<String>,
    /// Allowed `Host` header values, or `None` when bound to a wildcard address.
    allowed_hosts: Option<HashSet<String>>,
    /// Live MCP sessions (id → last activity).
    sessions: Arc<SessionStore>,
    /// When true, skip all session bookkeeping (`--stateless`).
    stateless: bool,
}

/// Serve the MCP server (+ REST) over HTTP at `addr`. `api_key`, when `Some` and
/// non-blank, is required on every request. Stateful sessions are on by default,
/// with a background idle reaper.
pub async fn serve_http(
    server: Server,
    addr: SocketAddr,
    api_key: Option<String>,
) -> std::io::Result<()> {
    let api_key = api_key.filter(|k| !k.trim().is_empty());
    let state = HttpState {
        server: Arc::new(RwLock::new(server)),
        api_key,
        allowed_hosts: host_allowlist(&addr),
        sessions: Arc::new(SessionStore::new()),
        stateless: false,
    };
    spawn_reaper(state.sessions.clone());
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router(state)).await
}

/// Periodically drop sessions idle longer than [`DEFAULT_SESSION_IDLE`].
fn spawn_reaper(sessions: Arc<SessionStore>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            sessions.reap(Instant::now(), DEFAULT_SESSION_IDLE);
        }
    });
}

fn router(state: HttpState) -> Router {
    Router::new()
        .route(
            "/mcp",
            post(handle_post).get(handle_sse).delete(handle_delete),
        )
        // REST surface (C3d): read-only JSON wrappers over the engine.
        .route("/api/stats", get(rest_stats))
        .route("/api/god-nodes", get(rest_god_nodes))
        .route("/api/node/:label", get(rest_node))
        .route("/api/query", get(rest_query))
        .route("/api/repos", get(rest_repos))
        .with_state(state)
}

/// When bound to a specific/loopback address, only accept these `Host` headers
/// (DNS-rebinding protection). A wildcard bind (`0.0.0.0`/`::`) disables it —
/// that's an intentional public exposure.
fn host_allowlist(addr: &SocketAddr) -> Option<HashSet<String>> {
    if addr.ip().is_unspecified() {
        return None;
    }
    let port = addr.port();
    let mut set = HashSet::new();
    for h in ["localhost", "127.0.0.1", "[::1]", &addr.ip().to_string()] {
        set.insert(h.to_string());
        set.insert(format!("{h}:{port}"));
    }
    Some(set)
}

/// Extract the `host[:port]` authority from an `Origin` header value by
/// stripping the `scheme://` prefix. Values without a scheme (e.g. the literal
/// `null` sent by sandboxed/privacy-sensitive browsers) are returned unchanged
/// so they fail the allowlist check.
fn origin_authority(origin: &str) -> &str {
    origin
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(origin)
}

/// Host-allowlist + Origin-allowlist + API-key gate shared by every route.
/// Returns the rejection response when a check fails, else `None` (request may
/// proceed).
fn guard(headers: &HeaderMap, st: &HttpState) -> Option<Response> {
    if let Some(allowed) = &st.allowed_hosts {
        let host = headers
            .get("host")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        if !allowed.contains(host) {
            return Some((StatusCode::FORBIDDEN, "forbidden host").into_response());
        }
        // DNS-rebinding protection (2025-11-25): reject a present-but-disallowed
        // Origin. Absent Origin (non-browser clients) is allowed. Gated on the
        // same specific/loopback bind as the Host check; a wildcard bind
        // disables both.
        if let Some(origin) = headers.get("origin").and_then(|h| h.to_str().ok()) {
            if !allowed.contains(origin_authority(origin)) {
                return Some((StatusCode::FORBIDDEN, "forbidden origin").into_response());
            }
        }
    }
    if let Some(key) = &st.api_key {
        if !authorized(headers, key) {
            return Some(
                (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({ "error": "unauthorized" })),
                )
                    .into_response(),
            );
        }
    }
    None
}

/// Validate the `MCP-Protocol-Version` header (Streamable HTTP, 2025-11-25): a
/// present-but-unsupported value MUST be answered with 400 Bad Request. An absent
/// header is tolerated (the spec says assume `2025-03-26` for backwards
/// compatibility); the `initialize` request is exempt (its version comes from
/// negotiation, not this header), so callers skip the check there.
fn protocol_version_rejection(headers: &HeaderMap) -> Option<Response> {
    let value = headers
        .get("mcp-protocol-version")
        .and_then(|h| h.to_str().ok())?;
    if crate::SUPPORTED_PROTOCOLS.contains(&value) {
        None
    } else {
        Some((StatusCode::BAD_REQUEST, "unsupported MCP-Protocol-Version").into_response())
    }
}

fn session_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get("mcp-session-id")
        .and_then(|h| h.to_str().ok())
        .map(str::to_string)
}

/// `POST /mcp` — one JSON-RPC request.
async fn handle_post(State(st): State<HttpState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(resp) = guard(&headers, &st) {
        return resp;
    }
    let Ok(req) = serde_json::from_slice::<Value>(&body) else {
        return (StatusCode::BAD_REQUEST, "invalid JSON").into_response();
    };
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");

    // MCP-Protocol-Version header (2025-11-25): a present-but-unsupported value
    // MUST get 400 on any post-initialization request. `initialize` is exempt
    // (its version comes from negotiation); an absent header is tolerated.
    if method != "initialize" {
        if let Some(resp) = protocol_version_rejection(&headers) {
            return resp;
        }
    }

    let mut new_session: Option<String> = None;
    if !st.stateless {
        if method == "initialize" {
            new_session = Some(st.sessions.create());
        } else if let Some(id) = session_header(&headers) {
            // A supplied session id must be live; unknown/expired -> re-initialize.
            if !st.sessions.touch(&id) {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "unknown or expired session" })),
                )
                    .into_response();
            }
        }
        // A missing id on a non-initialize request is tolerated.
    }

    // Dispatch off the async executor (blocking PR tools must not stall the
    // runtime), under a shared read lock so concurrent reads don't serialize.
    // Reload only when graph.json actually changed; brief write lock.
    let needs_reload = matches!(method, "tools/call" | "resources/read");
    let server = st.server.clone();
    let Ok((reloaded, resp)) = tokio::task::spawn_blocking(move || {
        let mut reloaded = false;
        if needs_reload && read_server(&server).is_stale() {
            write_server(&server).maybe_reload();
            reloaded = true;
        }
        (reloaded, read_server(&server).dispatch_request(&req))
    })
    .await
    else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "internal error" })),
        )
            .into_response();
    };
    // The graph (and thus every resource's content) changed: push to subscribers.
    if reloaded {
        st.sessions.notify_all_resources_changed();
    }
    match resp {
        Some(v) => match new_session {
            Some(id) => (StatusCode::OK, [("mcp-session-id", id)], Json(v)).into_response(),
            None => (StatusCode::OK, Json(v)).into_response(),
        },
        None => StatusCode::ACCEPTED.into_response(), // notification, no body
    }
}

/// `GET /mcp` — open the server→client SSE stream: keep-alive heartbeat plus
/// `notifications/resources/updated` pushes when the graph reloads (a tracked
/// session subscribes to its broadcast channel).
async fn handle_sse(State(st): State<HttpState>, headers: HeaderMap) -> Response {
    if let Some(resp) = guard(&headers, &st) {
        return resp;
    }
    if let Some(resp) = protocol_version_rejection(&headers) {
        return resp;
    }
    let mut session_id = None;
    if !st.stateless {
        if let Some(id) = session_header(&headers) {
            if !st.sessions.touch(&id) {
                return (StatusCode::NOT_FOUND, "unknown or expired session").into_response();
            }
            session_id = Some(id);
        }
    }
    // Bounded so an abandoned (or sessionless) GET can't hold a connection for
    // the process lifetime: ends once a tracked session is reaped, or after a
    // hard cap (~the idle timeout) of emitted events.
    const PING: Duration = Duration::from_secs(15);
    let max_events = (DEFAULT_SESSION_IDLE.as_secs() / PING.as_secs()).max(1);
    let sessions = st.sessions.clone();
    // A tracked session subscribes; a sessionless GET only heartbeats.
    let rx = session_id.as_ref().and_then(|id| sessions.subscribe(id));
    let body = futures_util::stream::unfold((0u64, rx), move |(count, mut rx)| {
        let sessions = sessions.clone();
        let session_id = session_id.clone();
        async move {
            if count >= max_events {
                return None;
            }
            // End promptly once a tracked session has been reaped.
            if let Some(id) = &session_id {
                if !sessions.contains(id) {
                    return None;
                }
            }
            let event = match rx.as_mut() {
                Some(r) => {
                    tokio::select! {
                        biased;
                        signal = r.recv() => match signal {
                            // Graph reloaded (or we lagged behind one): notify.
                            Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => {
                                resource_updated_event()
                            }
                            // Sender gone (session dropped): end the stream.
                            Err(broadcast::error::RecvError::Closed) => return None,
                        },
                        _ = tokio::time::sleep(PING) => Event::default().comment("keep-alive"),
                    }
                }
                None => {
                    tokio::time::sleep(PING).await;
                    Event::default().comment("keep-alive")
                }
            };
            Some((Ok::<_, Infallible>(event), (count + 1, rx)))
        }
    });
    Sse::new(body)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// The server-initiated JSON-RPC notification telling a client a subscribed
/// resource changed. The graph reload changes every resource's content; the
/// stats URI is a representative signal to re-read.
fn resource_updated_event() -> Event {
    let note = json!({
        "jsonrpc": "2.0",
        "method": "notifications/resources/updated",
        "params": { "uri": "synaptic://stats" }
    });
    Event::default().data(note.to_string())
}

/// `DELETE /mcp` — terminate a session.
async fn handle_delete(State(st): State<HttpState>, headers: HeaderMap) -> Response {
    if let Some(resp) = guard(&headers, &st) {
        return resp;
    }
    if let Some(resp) = protocol_version_rejection(&headers) {
        return resp;
    }
    match session_header(&headers) {
        Some(id) if st.sessions.remove(&id) => StatusCode::NO_CONTENT.into_response(),
        Some(_) => (StatusCode::NOT_FOUND, "unknown session").into_response(),
        None => (StatusCode::BAD_REQUEST, "missing Mcp-Session-Id").into_response(),
    }
}

// REST surface (C3d): read-only JSON wrappers over the same engine calls the
// MCP tools use. Each returns `{ "text": <tool output> }` (the tools' output is
// load-bearing formatted text, so we pass it through verbatim).

fn text_json(text: String) -> Response {
    (StatusCode::OK, Json(json!({ "text": text }))).into_response()
}

async fn rest_stats(State(st): State<HttpState>, headers: HeaderMap) -> Response {
    if let Some(resp) = guard(&headers, &st) {
        return resp;
    }
    let server = st.server.clone();
    let Ok(text) =
        tokio::task::spawn_blocking(move || read_server(&server).tool_graph_stats()).await
    else {
        return internal_error();
    };
    text_json(text)
}

async fn rest_god_nodes(
    State(st): State<HttpState>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if let Some(resp) = guard(&headers, &st) {
        return resp;
    }
    let top_n = q
        .get("top_n")
        .and_then(|v| v.parse().ok())
        .unwrap_or(10usize);
    let server = st.server.clone();
    let Ok(text) =
        tokio::task::spawn_blocking(move || read_server(&server).tool_god_nodes(top_n, 0)).await
    else {
        return internal_error();
    };
    text_json(text)
}

async fn rest_repos(
    State(st): State<HttpState>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if let Some(resp) = guard(&headers, &st) {
        return resp;
    }
    // `?repo=<tag>` -> one member's stats; otherwise list all members.
    let repo = q.get("repo").cloned();
    let server = st.server.clone();
    let Ok(text) = tokio::task::spawn_blocking(move || match repo {
        Some(repo) => read_server(&server).tool_repo_stats(&repo),
        None => read_server(&server).tool_list_repos(),
    })
    .await
    else {
        return internal_error();
    };
    text_json(text)
}

async fn rest_node(
    State(st): State<HttpState>,
    headers: HeaderMap,
    Path(label): Path<String>,
) -> Response {
    if let Some(resp) = guard(&headers, &st) {
        return resp;
    }
    let server = st.server.clone();
    let Ok(text) =
        tokio::task::spawn_blocking(move || read_server(&server).tool_get_node(&label)).await
    else {
        return internal_error();
    };
    text_json(text)
}

async fn rest_query(
    State(st): State<HttpState>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if let Some(resp) = guard(&headers, &st) {
        return resp;
    }
    let Some(question) = q.get("q") else {
        return (StatusCode::BAD_REQUEST, "missing ?q=").into_response();
    };
    // token_budget maps to a node cap inside the tool (≈ budget/40, clamped).
    let token_budget = q
        .get("token_budget")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1200usize);
    let question = question.clone();
    let server = st.server.clone();
    let Ok(text) = tokio::task::spawn_blocking(move || {
        read_server(&server).tool_query_graph(
            &question,
            synaptic_query::TraversalMode::Bfs,
            token_budget,
            &[],
        )
    })
    .await
    else {
        return internal_error();
    };
    text_json(text)
}

/// True if the request carries the right key via `X-API-Key` or
/// `Authorization: Bearer <key>` (scheme case-insensitive, RFC 6750).
fn authorized(headers: &HeaderMap, key: &str) -> bool {
    let supplied = headers
        .get("x-api-key")
        .and_then(|h| h.to_str().ok())
        .map(str::to_string)
        .or_else(|| {
            let auth = headers.get("authorization")?.to_str().ok()?;
            let (scheme, rest) = auth.split_once(' ')?;
            scheme
                .eq_ignore_ascii_case("bearer")
                .then(|| rest.trim().to_string())
        });
    match supplied {
        Some(s) => constant_time_eq(s.as_bytes(), key.as_bytes()),
        None => false,
    }
}

/// Length-then-content constant-time comparison (no early-exit on content).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::Map;
    use synaptic_core::GraphData;
    use tower::ServiceExt;

    fn test_state(api_key: Option<&str>) -> HttpState {
        let gd = GraphData {
            directed: false,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![],
            links: vec![],
            hyperedges: vec![],
            built_at_commit: None,
        };
        HttpState {
            server: Arc::new(RwLock::new(Server::from_graph_data(gd, None))),
            api_key: api_key.map(str::to_string),
            allowed_hosts: None, // wildcard: no host check in tests
            sessions: Arc::new(SessionStore::new()),
            stateless: false,
        }
    }

    fn state_with_server(server: Server) -> HttpState {
        HttpState {
            server: Arc::new(RwLock::new(server)),
            api_key: None,
            allowed_hosts: None, // wildcard: no host check in tests
            sessions: Arc::new(SessionStore::new()),
            stateless: false,
        }
    }

    fn test_state_loopback() -> HttpState {
        let mut st = test_state(None);
        st.allowed_hosts = host_allowlist(&"127.0.0.1:8080".parse().unwrap());
        st
    }

    fn init_body() -> Body {
        Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
    }

    /// C1: a slow PR tool (blocking `gh`/`git` subprocess) must NOT serialize
    /// other requests. With the old single `Mutex<Server>` held across the
    /// blocking call, a concurrent read blocks behind it; with a read lock +
    /// off-executor dispatch, the read proceeds. Many worker threads so the RED
    /// case fails cleanly (times out) instead of starving the runtime.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn slow_pr_tool_does_not_serialize_other_requests() {
        use std::sync::{Condvar, Mutex as StdMutex};
        use tokio::sync::oneshot;

        // A gh/git runner whose first call blocks until released, signaling once
        // when it begins blocking (so the test knows the server is now "busy").
        struct GateRunner {
            started: StdMutex<Option<oneshot::Sender<()>>>,
            release: Arc<(StdMutex<bool>, Condvar)>,
        }
        impl synaptic_prs::CommandRunner for GateRunner {
            fn run(&self, _program: &str, _args: &[&str]) -> Option<String> {
                if let Some(tx) = self.started.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                let (lock, cv) = &*self.release;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = cv.wait(released).unwrap();
                }
                Some("[]".to_string()) // empty `gh pr list`
            }
        }

        let (started_tx, started_rx) = oneshot::channel();
        let release = Arc::new((StdMutex::new(false), Condvar::new()));
        let runner = GateRunner {
            started: StdMutex::new(Some(started_tx)),
            release: release.clone(),
        };

        let gd = GraphData {
            directed: false,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![],
            links: vec![],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let server = Server::from_graph_data(gd, None).with_runner(Box::new(runner));
        let state = state_with_server(server);

        // Fire a slow triage_prs; `base` is supplied so it skips the default-branch
        // lookup and blocks at the first `gh pr list`, holding the server.
        let triage = tokio::spawn(router(state.clone()).oneshot(
            Request::post("/mcp")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"triage_prs","arguments":{"base":"main"}}}"#,
                ))
                .unwrap(),
        ));

        // Wait until triage is actually blocked inside the runner.
        started_rx.await.unwrap();

        // A concurrent read (graph_stats) MUST complete without waiting for triage.
        // Run it on its own task so the timeout can fire even if the handler blocks
        // synchronously (the very serialization bug under test).
        let stats_task = tokio::spawn(router(state.clone()).oneshot(
            Request::post("/mcp")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"graph_stats"}}"#,
                ))
                .unwrap(),
        ));
        let stats_result = tokio::time::timeout(Duration::from_secs(5), stats_task).await;

        // Release triage and clean up regardless of the assertion outcome.
        {
            let (lock, cv) = &*release;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }
        let _ = triage.await;

        let resp = stats_result
            .expect("a read request must not block behind a slow PR tool (server was serialized)")
            .expect("stats task panicked")
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn constant_time_eq_matches_and_rejects() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secrduff"));
        assert!(!constant_time_eq(b"secret", b"Secret"));
    }

    #[test]
    fn host_allowlist_off_for_wildcard_on_for_loopback() {
        assert!(host_allowlist(&"0.0.0.0:8080".parse().unwrap()).is_none());
        let set = host_allowlist(&"127.0.0.1:8080".parse().unwrap()).unwrap();
        assert!(set.contains("localhost:8080"));
        assert!(set.contains("127.0.0.1:8080"));
    }

    #[test]
    fn origin_authority_strips_scheme() {
        assert_eq!(origin_authority("http://localhost:8080"), "localhost:8080");
        assert_eq!(origin_authority("https://127.0.0.1:8080"), "127.0.0.1:8080");
        assert_eq!(origin_authority("http://localhost"), "localhost");
        // A bare/odd value with no scheme is returned unchanged (will fail the
        // allowlist check downstream, which is what we want).
        assert_eq!(origin_authority("null"), "null");
        assert_eq!(origin_authority("evil.com"), "evil.com");
    }

    #[tokio::test]
    async fn rejects_disallowed_origin() {
        let resp = router(test_state_loopback())
            .oneshot(
                Request::post("/mcp")
                    .header("content-type", "application/json")
                    .header("host", "127.0.0.1:8080")
                    .header("origin", "http://evil.com")
                    .body(init_body())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn allows_loopback_origin() {
        let resp = router(test_state_loopback())
            .oneshot(
                Request::post("/mcp")
                    .header("content-type", "application/json")
                    .header("host", "127.0.0.1:8080")
                    .header("origin", "http://localhost:8080")
                    .body(init_body())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn allows_absent_origin() {
        // Non-browser MCP clients send no Origin header; must not be rejected.
        let resp = router(test_state_loopback())
            .oneshot(
                Request::post("/mcp")
                    .header("content-type", "application/json")
                    .header("host", "127.0.0.1:8080")
                    .body(init_body())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn no_auth_when_key_absent() {
        let resp = router(test_state(None))
            .oneshot(
                Request::post("/mcp")
                    .header("content-type", "application/json")
                    .body(init_body())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn rejects_missing_and_wrong_key_accepts_correct() {
        // Missing key -> 401.
        let r = router(test_state(Some("sk")))
            .oneshot(Request::post("/mcp").body(init_body()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

        // Wrong key -> 401.
        let r = router(test_state(Some("sk")))
            .oneshot(
                Request::post("/mcp")
                    .header("x-api-key", "nope")
                    .body(init_body())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

        // Correct key via X-API-Key -> 200.
        let r = router(test_state(Some("sk")))
            .oneshot(
                Request::post("/mcp")
                    .header("x-api-key", "sk")
                    .body(init_body())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        // Correct key via Bearer (case-insensitive scheme) -> 200.
        let r = router(test_state(Some("sk")))
            .oneshot(
                Request::post("/mcp")
                    .header("authorization", "bEaReR sk")
                    .body(init_body())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn stateful_session_lifecycle() {
        let app = router(test_state(None));

        // initialize -> 200 + a fresh Mcp-Session-Id header.
        let r = app
            .clone()
            .oneshot(Request::post("/mcp").body(init_body()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let sid = r
            .headers()
            .get("mcp-session-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(sid.len(), 32);

        // A follow-up carrying that id -> 200.
        let r = app
            .clone()
            .oneshot(
                Request::post("/mcp")
                    .header("mcp-session-id", &sid)
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        // A bogus session id -> 404 (client should re-initialize).
        let r = app
            .clone()
            .oneshot(
                Request::post("/mcp")
                    .header("mcp-session-id", "deadbeef")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);

        // DELETE terminates it (204); a second DELETE -> 404.
        let r = app
            .clone()
            .oneshot(
                Request::delete("/mcp")
                    .header("mcp-session-id", &sid)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        let r = app
            .oneshot(
                Request::delete("/mcp")
                    .header("mcp-session-id", &sid)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn sse_get_opens_event_stream() {
        let r = router(test_state(None))
            .oneshot(
                Request::get("/mcp")
                    .header("accept", "text/event-stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let ct = r
            .headers()
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        assert!(ct.starts_with("text/event-stream"), "content-type: {ct}");
    }

    /// End-to-end subscription push: open the SSE stream for a session, change
    /// graph.json on disk, fire a tools/call that triggers the hot-reload, and
    /// assert a `notifications/resources/updated` frame arrives on the stream.
    #[tokio::test]
    async fn sse_pushes_resource_updated_on_reload() {
        use futures_util::StreamExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.json");
        let g1 = r#"{"directed":false,"multigraph":false,"graph":{},"nodes":[{"id":"a","label":"A","file_type":"code","source_file":"a.py"}],"links":[],"hyperedges":[]}"#;
        std::fs::write(&path, g1).unwrap();

        let app = router(state_with_server(Server::load(path.clone()).unwrap()));

        // initialize -> session id.
        let init = app
            .clone()
            .oneshot(Request::post("/mcp").body(init_body()).unwrap())
            .await
            .unwrap();
        let sid = init
            .headers()
            .get("mcp-session-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        // Open the SSE stream (this subscribes the session to its channel).
        let sse = app
            .clone()
            .oneshot(
                Request::get("/mcp")
                    .header("accept", "text/event-stream")
                    .header("mcp-session-id", &sid)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(sse.status(), StatusCode::OK);

        // Change graph.json on disk (extra node -> different size, so is_stale).
        let g2 = r#"{"directed":false,"multigraph":false,"graph":{},"nodes":[{"id":"a","label":"A","file_type":"code","source_file":"a.py"},{"id":"b","label":"B","file_type":"code","source_file":"b.py"}],"links":[],"hyperedges":[]}"#;
        std::fs::write(&path, g2).unwrap();

        // A data request hot-reloads the graph and notifies subscribers.
        let _ = app
            .clone()
            .oneshot(
                Request::post("/mcp")
                    .header("mcp-session-id", &sid)
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"graph_stats"}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        // The broadcast buffers the signal, so the first SSE frame is the push.
        let mut stream = sse.into_body().into_data_stream();
        let frame = tokio::time::timeout(Duration::from_secs(3), stream.next())
            .await
            .expect("an SSE frame within 3s")
            .expect("stream not ended")
            .expect("frame bytes");
        let text = String::from_utf8_lossy(&frame);
        assert!(
            text.contains("notifications/resources/updated"),
            "expected a resource-updated push, got: {text}"
        );
    }

    #[tokio::test]
    async fn rest_routes_return_json_and_respect_auth() {
        // /api/stats with no key -> JSON text payload.
        let r = router(test_state(None))
            .oneshot(Request::get("/api/stats").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        // /api/query without ?q= -> 400.
        let r = router(test_state(None))
            .oneshot(Request::get("/api/query").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);

        // REST honours the API key too.
        let r = router(test_state(Some("sk")))
            .oneshot(Request::get("/api/stats").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        let r = router(test_state(Some("sk")))
            .oneshot(
                Request::get("/api/stats")
                    .header("x-api-key", "sk")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn rejects_unsupported_protocol_version_header() {
        // 2025-11-25 transport: a present-but-unsupported MCP-Protocol-Version on
        // a post-initialization request MUST be answered with 400 Bad Request.
        let resp = router(test_state(None))
            .oneshot(
                Request::post("/mcp")
                    .header("content-type", "application/json")
                    .header("mcp-protocol-version", "1999-01-01")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn sse_get_rejects_unsupported_protocol_version_header() {
        // The header check covers the GET (SSE) channel too.
        let resp = router(test_state(None))
            .oneshot(
                Request::get("/mcp")
                    .header("accept", "text/event-stream")
                    .header("mcp-protocol-version", "1999-01-01")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn accepts_supported_protocol_version_header() {
        // A negotiated/supported version passes through.
        let resp = router(test_state(None))
            .oneshot(
                Request::post("/mcp")
                    .header("content-type", "application/json")
                    .header("mcp-protocol-version", "2025-11-25")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn tolerates_absent_protocol_version_header() {
        // Backwards compatibility: an absent header is NOT rejected (the spec
        // says assume 2025-03-26), so simple request/response clients keep working.
        let resp = router(test_state(None))
            .oneshot(
                Request::post("/mcp")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn initialize_is_exempt_from_protocol_version_header() {
        // The version is negotiated in the initialize exchange, so the header is
        // not expected there; a bad/absent value must not block initialization.
        let resp = router(test_state(None))
            .oneshot(
                Request::post("/mcp")
                    .header("content-type", "application/json")
                    .header("mcp-protocol-version", "not-a-version")
                    .body(init_body())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}

//! In-memory MCP session store for the stateful Streamable-HTTP transport.
//!
//! This realizes the MCP Streamable-HTTP session model: an `initialize`
//! mints an opaque `Mcp-Session-Id`, later requests carry it (an unknown id ⇒
//! 404 ⇒ re-initialize), and an idle reaper drops sessions after a timeout
//! (`session_idle_timeout`, default 1h).
//!
//! Time is injected (`*_at` / `reap` take `Instant`) so the reaper is tested
//! deterministically without sleeping.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::broadcast;

/// Default idle timeout (reference default: 3600s).
pub const DEFAULT_SESSION_IDLE: Duration = Duration::from_secs(3600);

/// Per-session state: last activity (for the reaper) plus a broadcast channel
/// the transport listens on to push `notifications/resources/updated` when the
/// graph reloads.
struct Session {
    last: Instant,
    tx: broadcast::Sender<()>,
}

/// Thread-safe map of session id → `Session`.
#[derive(Default)]
pub struct SessionStore {
    inner: Mutex<HashMap<String, Session>>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquire the inner map, recovering the guard if the lock was poisoned by a
    /// prior panic. The data is still valid; a single panic must not poison the
    /// session store and take down every subsequent request.
    fn guard(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Mint a new opaque, unguessable session id (128 random bits, hex) and
    /// record it as active at `now`.
    pub fn create_at(&self, now: Instant) -> String {
        let id = random_id();
        let (tx, _) = broadcast::channel(8);
        self.guard().insert(id.clone(), Session { last: now, tx });
        id
    }

    /// [`create_at`](Self::create_at) using the real clock.
    pub fn create(&self) -> String {
        self.create_at(Instant::now())
    }

    /// Update a session's last-activity to `now`; returns false for unknown ids.
    pub fn touch_at(&self, id: &str, now: Instant) -> bool {
        let mut map = self.guard();
        if let Some(s) = map.get_mut(id) {
            s.last = now;
            true
        } else {
            false
        }
    }

    /// Subscribe to a session's resource-change signal, or `None` if unknown.
    /// The transport awaits this receiver and pushes a notification on each send.
    pub fn subscribe(&self, id: &str) -> Option<broadcast::Receiver<()>> {
        self.guard().get(id).map(|s| s.tx.subscribe())
    }

    /// Signal every live session that the graph (and thus its resources) changed.
    /// A session with no current subscriber simply drops the signal.
    pub fn notify_all_resources_changed(&self) {
        for s in self.guard().values() {
            let _ = s.tx.send(());
        }
    }

    /// [`touch_at`](Self::touch_at) using the real clock.
    pub fn touch(&self, id: &str) -> bool {
        self.touch_at(id, Instant::now())
    }

    /// Whether a session is currently live (read-only; does NOT touch it).
    pub fn contains(&self, id: &str) -> bool {
        self.guard().contains_key(id)
    }

    /// Remove a session; returns true if it existed.
    pub fn remove(&self, id: &str) -> bool {
        self.guard().remove(id).is_some()
    }

    /// Drop every session idle longer than `idle` as of `now`; returns how many
    /// were reaped.
    pub fn reap(&self, now: Instant, idle: Duration) -> usize {
        let mut map = self.guard();
        let before = map.len();
        map.retain(|_, s| now.saturating_duration_since(s.last) <= idle);
        before - map.len()
    }

    pub fn len(&self) -> usize {
        self.guard().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 128 bits of OS randomness, lowercase hex. Falls back to a (still unique)
/// nanosecond-derived id only if the RNG is somehow unavailable.
fn random_id() -> String {
    let mut buf = [0u8; 16];
    if getrandom::getrandom(&mut buf).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        buf[..16].copy_from_slice(&nanos.to_le_bytes());
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_touch_remove() {
        let s = SessionStore::new();
        let base = Instant::now();
        let id = s.create_at(base);
        assert_eq!(id.len(), 32, "128-bit hex id");
        assert!(s.touch_at(&id, base), "known id touches");
        assert!(!s.touch_at("nope", base), "unknown id does not");
        assert_eq!(s.len(), 1);
        assert!(s.remove(&id));
        assert!(!s.remove(&id));
        assert!(s.is_empty());
    }

    #[test]
    fn survives_poisoned_lock() {
        use std::sync::Arc;
        let s = Arc::new(SessionStore::new());
        let s2 = s.clone();
        // Poison the inner mutex by panicking while holding the lock.
        let _ = std::thread::spawn(move || {
            let _g = s2.inner.lock().unwrap();
            panic!("poison the session lock");
        })
        .join();
        // Every operation still works: the guard recovers the poisoned lock
        // instead of cascading the panic into all future requests.
        let id = s.create();
        assert!(s.contains(&id));
        assert!(s.touch(&id));
        assert_eq!(s.len(), 1);
        assert!(s.remove(&id));
        assert!(s.is_empty());
    }

    #[test]
    fn ids_are_unique() {
        let s = SessionStore::new();
        let a = s.create();
        let b = s.create();
        assert_ne!(a, b);
    }

    #[test]
    fn session_broadcast_delivers_to_subscriber() {
        let s = SessionStore::new();
        let id = s.create();
        let mut rx = s.subscribe(&id).expect("a live session subscribes");
        s.notify_all_resources_changed();
        // try_recv is synchronous: no runtime needed to verify delivery.
        assert!(
            rx.try_recv().is_ok(),
            "subscriber receives the change signal"
        );
        // Unknown id -> no receiver.
        assert!(s.subscribe("nope").is_none());
    }

    #[test]
    fn reap_drops_only_idle_sessions() {
        let s = SessionStore::new();
        let base = Instant::now();
        let id = s.create_at(base);
        let idle = Duration::from_secs(3600);
        // Within the window: kept.
        assert_eq!(s.reap(base + Duration::from_secs(60), idle), 0);
        assert_eq!(s.len(), 1);
        // Past the window: reaped.
        assert_eq!(s.reap(base + Duration::from_secs(7200), idle), 1);
        assert!(s.is_empty());
        // A touch resets the clock, sparing it from a later reap.
        let id2 = s.create_at(base);
        assert!(s.touch_at(&id2, base + Duration::from_secs(7000)));
        assert_eq!(s.reap(base + Duration::from_secs(7200), idle), 0);
        let _ = id;
    }
}

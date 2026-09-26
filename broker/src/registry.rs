//! In-memory registry of relays that have finished the RDCleanPath handshake.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat, Utc};
use dashmap::DashMap;
use serde::Serialize;
use tokio::sync::watch;
use uuid::Uuid;

/// One live relay. Byte counters and `last_activity` are atomics so the pipe
/// does not take the registry lock.
pub struct Session {
    pub id: Uuid,
    pub destination: String,
    pub client_ip: String,
    pub started_at: DateTime<Utc>,
    started: Instant,
    bytes_sent: AtomicU64,
    bytes_received: AtomicU64,
    last_activity_ms: AtomicI64,
    kill: watch::Sender<bool>,
}

impl Session {
    pub fn add_sent(&self, n: u64) {
        if n == 0 {
            return;
        }
        self.bytes_sent.fetch_add(n, Ordering::Relaxed);
        self.touch();
        metrics::counter!("racko_bytes_sent_total").increment(n);
    }

    pub fn add_received(&self, n: u64) {
        if n == 0 {
            return;
        }
        self.bytes_received.fetch_add(n, Ordering::Relaxed);
        self.touch();
        metrics::counter!("racko_bytes_received_total").increment(n);
    }

    fn touch(&self) {
        self.last_activity_ms
            .store(Utc::now().timestamp_millis(), Ordering::Relaxed);
    }

    /// Time since the last byte in either direction. Wall clock matches `last_activity`.
    pub fn idle_for(&self) -> Duration {
        let last = self.last_activity_ms.load(Ordering::Relaxed);
        let elapsed_ms = u64::try_from(Utc::now().timestamp_millis().saturating_sub(last)).unwrap_or(0);
        Duration::from_millis(elapsed_ms)
    }

    /// Same signal the management API uses: the pipe `select` ends and `Drop` deregisters.
    pub fn request_stop(&self) {
        let _ = self.kill.send(true);
    }
}

/// Held by the connection task. Dropping it removes the session, including
/// when the task is cancelled or the pipe returns.
pub struct LiveSession {
    pub session: Arc<Session>,
    pub kill_rx: watch::Receiver<bool>,
    registry: Arc<Registry>,
}

impl Drop for LiveSession {
    fn drop(&mut self) {
        self.registry.deregister(self.session.id, self.session.started.elapsed().as_secs_f64());
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub id: Uuid,
    pub dest: String,
    pub client_ip: String,
    pub started_at: String,
    pub duration_secs: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

pub struct Registry {
    sessions: DashMap<Uuid, Arc<Session>>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            sessions: DashMap::new(),
        }
    }

    pub fn register(self: &Arc<Self>, destination: String, client_ip: String) -> LiveSession {
        let id = Uuid::new_v4();
        let (kill, kill_rx) = watch::channel(false);
        let now = Utc::now();
        let session = Arc::new(Session {
            id,
            destination: destination.clone(),
            client_ip: client_ip.clone(),
            started_at: now,
            started: Instant::now(),
            bytes_sent: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
            last_activity_ms: AtomicI64::new(now.timestamp_millis()),
            kill,
        });
        self.sessions.insert(id, Arc::clone(&session));
        metrics::counter!("racko_sessions_total").increment(1);
        metrics::gauge!("racko_active_sessions").set(self.sessions.len() as f64);
        tracing::info!(session_id = %id, dest = %destination, client_ip = %client_ip, "session registered");
        LiveSession {
            session,
            kill_rx,
            registry: Arc::clone(self),
        }
    }

    /// Signal the relay task to close both sockets and exit. The task itself
    /// deregisters when it unwinds, so this does not leave a detached task.
    pub fn kill(&self, id: Uuid) -> bool {
        let Some(session) = self.sessions.get(&id) else {
            return false;
        };
        session.request_stop();
        tracing::info!(session_id = %id, "session killed");
        true
    }

    pub fn list(&self) -> Vec<SessionView> {
        let now = Utc::now();
        self.sessions
            .iter()
            .map(|entry| {
                let session = entry.value();
                let duration = (now - session.started_at).num_seconds();
                SessionView {
                    id: session.id,
                    dest: session.destination.clone(),
                    client_ip: session.client_ip.clone(),
                    started_at: session.started_at.to_rfc3339_opts(SecondsFormat::Secs, true),
                    duration_secs: u64::try_from(duration).unwrap_or(0),
                    bytes_sent: session.bytes_sent.load(Ordering::Relaxed),
                    bytes_received: session.bytes_received.load(Ordering::Relaxed),
                }
            })
            .collect()
    }

    fn deregister(&self, id: Uuid, duration_secs: f64) {
        if self.sessions.remove(&id).is_none() {
            return;
        }
        metrics::histogram!("racko_session_duration_seconds").record(duration_secs);
        metrics::gauge!("racko_active_sessions").set(self.sessions.len() as f64);
        tracing::info!(session_id = %id, duration_secs, "session deregistered");
    }
}

//! Durable forward queue + bounded worker.
//!
//! The inbound path acks GOWA 200 *before* the agent has seen the message (so a slow LLM turn can't
//! trip GOWA's 10s webhook timeout). That makes the forward a delivery obligation the shim now owns:
//! if the agent is down, returns non-2xx, or the shim crashes, GOWA will not redeliver — it already
//! got its 200. A spawned-task-that-only-logs would silently lose the message.
//!
//! [`ForwardQueue`] closes that gap with a tiny on-disk queue under `SHIM_QUEUE_DIR`:
//!
//! ```text
//! <queue>/pending/<hex(id)>.json   awaiting (or retrying) forward
//! <queue>/dead/<hex(id)>.json      exhausted retries — operator audit/replay target
//! ```
//!
//! - `enqueue` writes `pending/<hex>.json.tmp`, `sync_all`s best-effort, then atomically renames to
//!   `<hex>.json`. The filename is `hex(id)` so a message id can never escape the queue dir via path
//!   traversal, and the *same* id always maps to the *same* file → enqueue is idempotent.
//! - A bounded worker (a `Semaphore` caps concurrent forwards) drains `pending/`, calling
//!   `agent.forward` with bounded exponential-backoff retries. On a 2xx it deletes the file; on
//!   retry exhaustion it renames the file into `dead/` and logs the path. It drains on startup
//!   (recovering anything left by a crash), then wakes on a `Notify` per enqueue plus a periodic
//!   safety tick.

use std::{
    collections::HashSet,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{
    sync::{Notify, Semaphore},
    task::JoinHandle,
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{agent::ForwardOutcome, channel::ChannelRouter, error::DynError, model::Inbound};

/// A durable, id-keyed queue of inbound messages awaiting forward to the agent. Cheap to clone
/// (paths plus an `Arc<Notify>`); the same handle lives in `AppState` and in the worker.
#[derive(Clone)]
pub struct ForwardQueue {
    pending: PathBuf,
    dead: PathBuf,
    notify: Arc<Notify>,
}

impl ForwardQueue {
    /// Open (creating if absent) the `pending/` and `dead/` subdirectories under `queue_dir`.
    pub fn new(queue_dir: &Path) -> io::Result<Self> {
        let pending = queue_dir.join("pending");
        let dead = queue_dir.join("dead");
        fs::create_dir_all(&pending)?;
        fs::create_dir_all(&dead)?;
        Ok(Self {
            pending,
            dead,
            notify: Arc::new(Notify::new()),
        })
    }

    /// Persist `inbound` to `pending/`, durably and idempotently, then wake the worker. The write is
    /// tmp-file + `sync_all` + atomic rename so a crash mid-write never leaves a half-written
    /// `<hex>.json` a reader could choke on. Returns the underlying IO error to the caller, which
    /// should then *not* ack GOWA (let it retry) rather than drop the message.
    pub fn enqueue(&self, inbound: &Inbound) -> Result<(), DynError> {
        let stem = hex::encode(inbound.id.as_bytes());
        let final_path = self.pending.join(format!("{stem}.json"));
        let tmp_path = self.pending.join(format!("{stem}.json.tmp"));

        let bytes = serde_json::to_vec(inbound)?;
        let mut file = fs::File::create(&tmp_path)?;
        file.write_all(&bytes)?;
        let _ = file.sync_all(); // best-effort durability; not all filesystems honour it
        drop(file);
        fs::rename(&tmp_path, &final_path)?;

        self.notify.notify_one();
        Ok(())
    }

    /// Count of `*.json` files currently in `pending/` (test/observability helper).
    pub fn pending_len(&self) -> usize {
        count_json(&self.pending)
    }

    /// Count of `*.json` files currently in `dead/` (test/observability helper).
    pub fn dead_len(&self) -> usize {
        count_json(&self.dead)
    }
}

/// Tunables for the drain worker. Built from [`crate::config::Config`] in production; constructed
/// directly with tiny backoffs in tests.
#[derive(Clone)]
pub struct WorkerConfig {
    pub concurrency: usize,
    pub max_retries: u32,
    pub base_backoff: Duration,
    pub max_backoff: Duration,
    /// Safety re-drain interval, independent of the per-enqueue `Notify`.
    pub tick: Duration,
}

impl WorkerConfig {
    /// The safety tick and backoff ceiling are not worth their own env vars; only the operationally
    /// meaningful knobs (`concurrency`, `max_retries`, `base_backoff`) come from config.
    pub fn from_parts(concurrency: usize, max_retries: u32, base_backoff: Duration) -> Self {
        Self {
            concurrency: concurrency.max(1),
            max_retries,
            base_backoff,
            max_backoff: Duration::from_secs(60),
            tick: Duration::from_secs(30),
        }
    }
}

/// A handle to the running drain worker. Held by `main` so shutdown can cancel + await an orderly
/// drain; dropped (detached) by tests that just want it running.
pub struct ForwardWorker {
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

impl ForwardWorker {
    /// Spawn the worker loop. It drains `pending/` immediately (startup recovery), then loops on the
    /// queue's `Notify` plus a periodic tick until cancelled. The `router` routes each file to its
    /// channel's client by the persisted `Inbound.channel` label.
    pub fn spawn(queue: ForwardQueue, router: Arc<ChannelRouter>, config: WorkerConfig) -> Self {
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(worker_loop(queue, router, config, cancel.clone()));
        Self { cancel, handle }
    }

    /// Signal the worker to stop, then await its final drain (bounded by `timeout`). In-flight
    /// forwards finish; anything still only-pending is left on disk for the next startup drain.
    pub async fn shutdown(self, timeout: Duration) {
        self.cancel.cancel();
        match tokio::time::timeout(timeout, self.handle).await {
            Ok(_) => tracing::info!("forward worker drained"),
            Err(_) => tracing::warn!("forward worker drain timed out; exiting anyway"),
        }
    }
}

/// Shared state the loop threads into each spawned forward task.
struct Worker {
    queue: ForwardQueue,
    router: Arc<ChannelRouter>,
    config: WorkerConfig,
    semaphore: Arc<Semaphore>,
    /// Filenames currently being processed, so a tick/notify mid-retry can't double-spawn the same
    /// file. Removed when the task finishes.
    in_flight: Arc<Mutex<HashSet<String>>>,
    forwards: TaskTracker,
    cancel: CancellationToken,
}

async fn worker_loop(
    queue: ForwardQueue,
    router: Arc<ChannelRouter>,
    config: WorkerConfig,
    cancel: CancellationToken,
) {
    let worker = Worker {
        semaphore: Arc::new(Semaphore::new(config.concurrency.max(1))),
        in_flight: Arc::new(Mutex::new(HashSet::new())),
        forwards: TaskTracker::new(),
        queue,
        router,
        config,
        cancel: cancel.clone(),
    };

    worker.drain_pending(); // startup recovery
    loop {
        tokio::select! {
            _ = worker.queue.notify.notified() => {}
            _ = tokio::time::sleep(worker.config.tick) => {}
            _ = cancel.cancelled() => break,
        }
        worker.drain_pending();
    }

    // Flush anything enqueued just before shutdown, then wait for in-flight forwards to settle.
    worker.drain_pending();
    worker.forwards.close();
    worker.forwards.wait().await;
}

impl Worker {
    /// Spawn a bounded forward task for each not-already-in-flight `*.json` in `pending/`. Returns
    /// immediately; the `Semaphore` (acquired *inside* each task) is what actually bounds concurrency.
    fn drain_pending(&self) {
        let entries = match fs::read_dir(&self.queue.pending) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(%error, "failed to read pending queue dir");
                return;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue; // skip *.tmp and anything else
            }
            let Some(name) = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            {
                let mut guard = self.in_flight.lock().expect("in_flight poisoned");
                if !guard.insert(name.clone()) {
                    continue; // already being processed
                }
            }

            let semaphore = self.semaphore.clone();
            let router = self.router.clone();
            let dead = self.queue.dead.clone();
            let config = self.config.clone();
            let cancel = self.cancel.clone();
            let in_flight = self.in_flight.clone();
            self.forwards.spawn(async move {
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .expect("forward semaphore never closed");
                process_file(&path, &router, &dead, &config, &cancel).await;
                in_flight.lock().expect("in_flight poisoned").remove(&name);
            });
        }
    }
}

/// Forward one pending file with bounded backoff retries. On success the file is deleted; on retry
/// exhaustion or an unparseable file it is moved to `dead/`; on cancellation mid-backoff it is left
/// in `pending/` for the next startup drain.
async fn process_file(
    path: &Path,
    router: &ChannelRouter,
    dead: &Path,
    config: &WorkerConfig,
    cancel: &CancellationToken,
) {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        // Vanished (already handled by a racing pass) — nothing to do.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            tracing::warn!(?path, %error, "failed to read pending queue file");
            return;
        }
    };
    let inbound: Inbound = match serde_json::from_slice(&bytes) {
        Ok(inbound) => inbound,
        Err(error) => {
            tracing::warn!(?path, %error, "unparseable pending queue file; dead-lettering");
            move_to_dead(path, dead);
            return;
        }
    };

    // Route by the persisted channel label. A label no longer configured falls back to the default
    // client (with a warn) rather than dead-lettering — see `ChannelRouter::client_for`.
    let agent = router.client_for(&inbound.channel);

    let mut attempt: u32 = 0;
    loop {
        match agent.forward(&inbound).await {
            Ok(outcome) => {
                let _ = fs::remove_file(path);
                match outcome {
                    ForwardOutcome::Forwarded => {
                        tracing::info!(id = %inbound.id, chat = %inbound.chat_id, channel = %inbound.channel, "forwarded inbound to agent")
                    }
                    ForwardOutcome::SinkDropped => {
                        tracing::info!(id = %inbound.id, chat = %inbound.chat_id, channel = %inbound.channel, "debug-sink: inbound accepted and discarded (NOT forwarded)")
                    }
                }
                return;
            }
            Err(error) if attempt >= config.max_retries => {
                let dest = move_to_dead(path, dead);
                tracing::warn!(id = %inbound.id, %error, ?dest, "forward exhausted retries; dead-lettered");
                return;
            }
            Err(error) => {
                let backoff = backoff_for(attempt, config);
                attempt += 1;
                tracing::warn!(id = %inbound.id, attempt, %error, "forward to agent failed; will retry");
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = cancel.cancelled() => {
                        tracing::info!(id = %inbound.id, "shutdown during backoff; leaving pending for restart drain");
                        return;
                    }
                }
            }
        }
    }
}

/// `base * 2^attempt`, saturating and clamped to `max_backoff`.
fn backoff_for(attempt: u32, config: &WorkerConfig) -> Duration {
    let factor = 1u32.checked_shl(attempt.min(16)).unwrap_or(u32::MAX);
    config
        .base_backoff
        .saturating_mul(factor)
        .min(config.max_backoff)
}

/// Move a pending file into `dead/` under the same name. Returns the destination for logging; on
/// rename failure the file is left in place (it will be retried) and `None` is returned.
fn move_to_dead(path: &Path, dead: &Path) -> Option<PathBuf> {
    let name = path.file_name()?;
    let dest = dead.join(name);
    match fs::rename(path, &dest) {
        Ok(()) => Some(dest),
        Err(error) => {
            tracing::warn!(?path, %error, "failed to move file to dead-letter dir");
            None
        }
    }
}

fn count_json(dir: &Path) -> usize {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| {
                    entry.path().extension().and_then(|ext| ext.to_str()) == Some("json")
                })
                .count()
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn sample(id: &str) -> Inbound {
        Inbound {
            chat_id: "123@g.us".into(),
            sender: "61400111222@s.whatsapp.net".into(),
            body: "hello".into(),
            id: id.into(),
            is_from_me: false,
            mentioned: false,
            reply_to: None,
            quoted_body: None,
            channel: "default".into(),
            media: vec![],
            kind: crate::model::InboundKind::Message,
            reaction: None,
            reacted_message_id: None,
        }
    }

    /// A unique, never-cleaned temp dir per test (the OS reaps `/tmp`). Avoids a tempfile dep and
    /// keeps concurrent tests from colliding.
    fn unique_dir() -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("wagw-fq-{}-{}", std::process::id(), n))
    }

    #[test]
    fn enqueue_writes_one_hex_named_json() {
        let dir = unique_dir();
        let queue = ForwardQueue::new(&dir).unwrap();
        queue.enqueue(&sample("MSG_1")).unwrap();

        assert_eq!(queue.pending_len(), 1);
        let stem = hex::encode(b"MSG_1");
        assert!(dir.join("pending").join(format!("{stem}.json")).exists());
        // No leftover tmp file.
        assert!(
            !dir.join("pending")
                .join(format!("{stem}.json.tmp"))
                .exists()
        );
    }

    #[test]
    fn enqueue_same_id_is_idempotent() {
        let dir = unique_dir();
        let queue = ForwardQueue::new(&dir).unwrap();
        queue.enqueue(&sample("DUP")).unwrap();
        queue.enqueue(&sample("DUP")).unwrap();
        // Same id → same filename → one queue item (so a duplicate webhook yields one forward).
        assert_eq!(queue.pending_len(), 1);
    }

    #[test]
    fn hex_filename_contains_no_path_separators() {
        // A message id full of traversal characters must not escape the queue dir.
        let dir = unique_dir();
        let queue = ForwardQueue::new(&dir).unwrap();
        let evil = "../../etc/passwd";
        queue.enqueue(&sample(evil)).unwrap();
        let stem = hex::encode(evil.as_bytes());
        assert!(!stem.contains('/') && !stem.contains('.'));
        assert!(dir.join("pending").join(format!("{stem}.json")).exists());
        assert_eq!(queue.pending_len(), 1);
    }

    #[test]
    fn enqueued_file_round_trips_to_the_same_inbound() {
        let dir = unique_dir();
        let queue = ForwardQueue::new(&dir).unwrap();
        let original = sample("RT");
        queue.enqueue(&original).unwrap();
        let stem = hex::encode(b"RT");
        let bytes = fs::read(dir.join("pending").join(format!("{stem}.json"))).unwrap();
        let restored: Inbound = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(restored, original);
    }

    #[test]
    fn backoff_grows_then_clamps() {
        let config = WorkerConfig::from_parts(4, 5, Duration::from_millis(100));
        assert_eq!(backoff_for(0, &config), Duration::from_millis(100));
        assert_eq!(backoff_for(1, &config), Duration::from_millis(200));
        assert_eq!(backoff_for(2, &config), Duration::from_millis(400));
        // Large attempt clamps to max_backoff rather than overflowing.
        assert_eq!(backoff_for(30, &config), config.max_backoff);
    }
}

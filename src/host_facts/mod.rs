//! Host facts worker: one background thread per server that refreshes a
//! `HostFacts` snapshot on a tiered beat, publishes it to
//! `state_dir/host-facts.json` by atomic rename, and serves it to `host.facts`
//! API calls and in-process readers. Probes never run on the App/UI thread.

mod collect;
mod paths;
mod probe;
mod tools;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::api::schema::host::HostFacts;
use crate::api::schema::{ErrorBody, ErrorResponse, ResponseResult, SuccessResponse};

pub(crate) use collect::Collector;
pub(crate) use probe::{HostProbes, SystemProbes};

/// Env var naming the facts file inside pane processes.
pub(crate) const HOST_FACTS_ENV_VAR: &str = "HERDR_HOST_FACTS";
pub(crate) const HOST_FACTS_FILE_NAME: &str = "host-facts.json";
pub(crate) const DEFAULT_INTERVAL_SECS: u64 = 60;
pub(crate) const MIN_INTERVAL_SECS: u64 = 30;
/// How long a `host.facts` call waits for the first or a forced refresh.
const API_WAIT: Duration = Duration::from_secs(30);

static ENABLED: AtomicBool = AtomicBool::new(false);
static SERVICE: OnceLock<Arc<Shared>> = OnceLock::new();
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct Shared {
    state: Mutex<SharedState>,
    changed: Condvar,
}

#[derive(Default)]
struct SharedState {
    snapshot: Option<Arc<HostFacts>>,
    generation: u64,
    refresh_requested: bool,
    stop: bool,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, SharedState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Clamps a configured interval to the supported minimum.
pub(crate) fn effective_interval_secs(configured: u64) -> u64 {
    configured.max(MIN_INTERVAL_SECS)
}

/// `state_dir/host-facts.json`.
pub(crate) fn facts_file_path() -> PathBuf {
    crate::config::state_dir().join(HOST_FACTS_FILE_NAME)
}

/// Facts file path for pane environments, when the worker is enabled.
pub(crate) fn pane_env_path() -> Option<PathBuf> {
    ENABLED.load(Ordering::Relaxed).then(facts_file_path)
}

/// Latest snapshot, read-only. `None` before the first refresh or when disabled.
/// In-process accessor for server-side readers (the host facts panel, U3).
#[allow(dead_code)]
pub(crate) fn current() -> Option<Arc<HostFacts>> {
    SERVICE
        .get()
        .and_then(|shared| shared.lock().snapshot.clone())
}

/// Stops the worker when dropped. The thread is not joined: a probe in flight
/// finishes within its own timeout and the thread then exits.
pub(crate) struct HostFactsWorker {
    shared: Arc<Shared>,
}

impl Drop for HostFactsWorker {
    fn drop(&mut self) {
        ENABLED.store(false, Ordering::Relaxed);
        self.shared.lock().stop = true;
        self.shared.changed.notify_all();
    }
}

/// Starts the worker when `[host_facts] enabled` (the default).
pub(crate) fn start(config: &crate::config::HostFactsConfig) -> Option<HostFactsWorker> {
    if !config.enabled {
        return None;
    }
    let interval = Duration::from_secs(effective_interval_secs(config.interval_secs));
    let shared = Arc::clone(SERVICE.get_or_init(|| Arc::new(Shared::default())));
    shared.lock().stop = false;
    let state_dir = crate::config::state_dir();
    let worker_shared = Arc::clone(&shared);
    let spawned = crate::thread_spawn::spawn_named("herdr-host-facts", move || {
        run_worker(&worker_shared, &SystemProbes, interval, &state_dir);
    });
    match spawned {
        Ok(_) => {
            ENABLED.store(true, Ordering::Relaxed);
            Some(HostFactsWorker { shared })
        }
        Err(err) => {
            tracing::warn!(err = %err, "failed to spawn host facts worker");
            None
        }
    }
}

fn run_worker(shared: &Shared, probes: &dyn HostProbes, interval: Duration, state_dir: &Path) {
    let started = Instant::now();
    // The disk probe reads the state directory's volume, so make sure the
    // directory exists before the first beat.
    if let Err(err) = std::fs::create_dir_all(state_dir) {
        tracing::debug!(err = %err, "failed to create state dir for host facts");
    }
    let mut collector = Collector::new(probes, interval.as_secs(), state_dir.to_path_buf());
    let mut force = false;
    loop {
        let mono_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let facts = Arc::new(collector.collect(probes, mono_ms, wall_clock_ms(), force));
        if let Err(err) = write_facts_file(state_dir, &facts) {
            tracing::debug!(err = %err, "failed to write host facts file");
        }
        let mut state = shared.lock();
        state.snapshot = Some(facts);
        state.generation = state.generation.wrapping_add(1);
        shared.changed.notify_all();

        let deadline = Instant::now() + interval;
        loop {
            if state.stop {
                return;
            }
            if state.refresh_requested {
                state.refresh_requested = false;
                force = true;
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                force = false;
                break;
            }
            state = shared
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

fn wall_clock_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Writes `dir/host-facts.json` through a unique temp file and an atomic
/// replace, so readers never see a partial file and concurrent servers do
/// not need a lock.
pub(crate) fn write_facts_file(dir: &Path, facts: &HostFacts) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let json = serde_json::to_vec_pretty(facts).map_err(std::io::Error::other)?;
    let temp = dir.join(format!(
        "{HOST_FACTS_FILE_NAME}.{}.{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = std::fs::write(&temp, &json)
        .and_then(|()| crate::platform::replace_file(&temp, &dir.join(HOST_FACTS_FILE_NAME)));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

pub(crate) fn read_facts_file(path: &Path) -> std::io::Result<HostFacts> {
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
}

/// Answers `host.facts` on the API connection thread. With `refresh`, wakes
/// the worker and waits (bounded) for the next snapshot.
pub(crate) fn api_response(id: String, refresh: bool) -> String {
    let Some(shared) = SERVICE.get().filter(|_| ENABLED.load(Ordering::Relaxed)) else {
        return error_json(
            id,
            "host_facts_disabled",
            "host facts are disabled; set [host_facts] enabled = true and restart the server",
        );
    };
    match wait_for_snapshot(shared, refresh, API_WAIT) {
        Some(facts) => serde_json::to_string(&SuccessResponse {
            id,
            result: ResponseResult::HostFacts(Box::new((*facts).clone())),
        })
        .unwrap_or_else(|_| {
            r#"{"id":"","error":{"code":"internal_error","message":"failed to encode response"}}"#
                .to_string()
        }),
        None => error_json(
            id,
            "host_facts_pending",
            "host facts are not ready yet; try again shortly",
        ),
    }
}

fn wait_for_snapshot(shared: &Shared, refresh: bool, wait: Duration) -> Option<Arc<HostFacts>> {
    let deadline = Instant::now() + wait;
    let mut state = shared.lock();
    let start_generation = state.generation;
    if refresh {
        state.refresh_requested = true;
        shared.changed.notify_all();
    }
    loop {
        let fresh_enough = if refresh {
            state.generation != start_generation
        } else {
            state.snapshot.is_some()
        };
        if fresh_enough || state.stop {
            return state.snapshot.clone();
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return state.snapshot.clone();
        }
        state = shared
            .changed
            .wait_timeout(state, remaining)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
}

fn error_json(id: String, code: &str, message: &str) -> String {
    serde_json::to_string(&ErrorResponse {
        id,
        error: ErrorBody {
            code: code.into(),
            message: message.into(),
        },
    })
    .unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests;

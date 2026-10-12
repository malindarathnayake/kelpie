//! Fork-only supervisor facts surface (read-only in step 2). Agent-neutral: any supervisor
//! (Dot, Hermes, OpenClaw, scripts) uses the same methods. Disabled unless `[supervisor] enabled`.

pub mod registry;
// Later units consume the re-export.
#[allow(unused_imports)]
pub use registry::SupervisorState;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

static ENABLED: OnceLock<bool> = OnceLock::new();
static SERVER_EPOCH: OnceLock<String> = OnceLock::new();
static TRACE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Set the process-global enable flag. A second call is ignored.
pub fn set_enabled(enabled: bool) {
    let _ = ENABLED.set(enabled);
}

#[cfg(test)]
thread_local! {
    static TEST_ENABLED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Test-only per-thread override of the enable flag. `None` restores the global value.
#[cfg(test)]
pub fn set_enabled_for_test(value: Option<bool>) {
    TEST_ENABLED.with(|c| c.set(value));
}

/// Whether the supervisor surface is enabled. False when never set.
pub fn enabled() -> bool {
    #[cfg(test)]
    if let Some(v) = TEST_ENABLED.with(|c| c.get()) {
        return v;
    }
    ENABLED.get().copied().unwrap_or(false)
}

/// 32 lowercase hex chars, fixed for the lifetime of the process.
pub fn server_epoch() -> &'static str {
    SERVER_EPOCH.get_or_init(|| {
        let local = 0u8;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut hasher = Sha256::new();
        hasher.update(std::process::id().to_le_bytes());
        hasher.update(nanos.to_le_bytes());
        hasher.update((&local as *const u8 as usize).to_le_bytes());
        hasher.update(format!("{:?}", std::time::Instant::now()).as_bytes());
        let digest = hasher.finalize();
        digest[..16].iter().map(|b| format!("{b:02x}")).collect()
    })
}

/// Process-unique trace id: first 8 chars of the epoch, then a 16-hex counter.
// Later units attach trace ids to supervisor responses.
#[allow(dead_code)]
pub fn next_trace_id() -> String {
    let n = TRACE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{:016x}", &server_epoch()[..8], n)
}

/// Milliseconds since the UNIX epoch; 0 on clock error.
// Later units timestamp supervisor facts.
#[allow(dead_code)]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_epoch_is_32_lowercase_hex_and_stable() {
        let a = server_epoch();
        let b = server_epoch();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
        assert_eq!(a, b);
    }

    #[test]
    fn trace_ids_are_distinct_and_prefixed_with_epoch() {
        let a = next_trace_id();
        let b = next_trace_id();
        assert_ne!(a, b);
        let prefix = format!("{}-", &server_epoch()[..8]);
        for id in [&a, &b] {
            assert!(id.starts_with(&prefix));
            let suffix = &id[prefix.len()..];
            assert_eq!(suffix.len(), 16);
            assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn now_ms_is_after_2020() {
        assert!(now_ms() > 1_577_836_800_000);
    }

    #[test]
    fn enabled_defaults_to_false_when_unset() {
        assert!(!enabled());
    }

    #[test]
    fn enabled_for_test_override_is_thread_local() {
        set_enabled_for_test(Some(true));
        assert!(enabled());
        std::thread::spawn(|| assert!(!enabled()))
            .join()
            .expect("thread panicked");
        set_enabled_for_test(None);
        assert!(!enabled());
    }
}

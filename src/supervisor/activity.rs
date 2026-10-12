//! Fork-only per-pane activity stamps (Q1). Written lock-free by the PTY reader thread and
//! the headless input path; read by `agents.digest`. Stores are gated by `enabled()`.

use std::sync::atomic::{AtomicU64, Ordering};

/// Unix-ms stamps; 0 means "never observed".
#[derive(Debug, Default)]
pub struct PaneActivity {
    last_output_ms: AtomicU64,
    last_owner_input_ms: AtomicU64,
}

impl PaneActivity {
    /// PTY reader: once per read batch. One OnceLock load when disabled; otherwise one
    /// `SystemTime::now()` and one Relaxed store. No allocation, no lock.
    pub fn record_output(&self) {
        if !crate::supervisor::enabled() {
            return;
        }
        self.last_output_ms
            .store(crate::supervisor::now_ms(), Ordering::Relaxed);
    }

    /// Client keyboard input forwarded to the pane. Same cost profile.
    pub fn record_owner_input(&self) {
        if !crate::supervisor::enabled() {
            return;
        }
        self.last_owner_input_ms
            .store(crate::supervisor::now_ms(), Ordering::Relaxed);
    }

    pub fn last_output_ms(&self) -> Option<u64> {
        match self.last_output_ms.load(Ordering::Relaxed) {
            0 => None,
            v => Some(v),
        }
    }

    pub fn last_owner_input_ms(&self) -> Option<u64> {
        match self.last_owner_input_ms.load(Ordering::Relaxed) {
            0 => None,
            v => Some(v),
        }
    }
}

/// Pure: max of the observed stamps; None when all are None.
pub fn last_activity_ms(
    output: Option<u64>,
    owner_input: Option<u64>,
    last_transition: Option<u64>,
) -> Option<u64> {
    output
        .into_iter()
        .chain(owner_input)
        .chain(last_transition)
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::{now_ms, set_enabled_for_test};

    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            set_enabled_for_test(None);
        }
    }

    #[test]
    fn last_activity_ms_table() {
        assert_eq!(last_activity_ms(None, None, None), None);
        assert_eq!(last_activity_ms(Some(4), None, None), Some(4));
        assert_eq!(last_activity_ms(None, Some(6), None), Some(6));
        assert_eq!(last_activity_ms(None, None, Some(8)), Some(8));
        assert_eq!(last_activity_ms(Some(5), Some(9), Some(7)), Some(9));
        assert_eq!(last_activity_ms(Some(10), Some(9), Some(7)), Some(10));
        assert_eq!(last_activity_ms(Some(5), Some(9), Some(70)), Some(70));
    }

    #[test]
    fn fresh_activity_reports_none() {
        let a = PaneActivity::default();
        assert_eq!(a.last_output_ms(), None);
        assert_eq!(a.last_owner_input_ms(), None);
    }

    #[test]
    fn record_output_stamps_only_output_when_enabled() {
        let _reset = Reset;
        set_enabled_for_test(Some(true));
        let a = PaneActivity::default();
        let before = now_ms();
        a.record_output();
        let v = a.last_output_ms().expect("output stamp set");
        assert!(v > 0);
        assert!(v >= before);
        assert_eq!(a.last_owner_input_ms(), None);
    }

    #[test]
    fn record_owner_input_stamps_only_input_when_enabled() {
        let _reset = Reset;
        set_enabled_for_test(Some(true));
        let a = PaneActivity::default();
        let before = now_ms();
        a.record_owner_input();
        let v = a.last_owner_input_ms().expect("input stamp set");
        assert!(v > 0);
        assert!(v >= before);
        assert_eq!(a.last_output_ms(), None);
    }

    #[test]
    fn disabled_records_nothing() {
        let _reset = Reset;
        set_enabled_for_test(Some(false));
        let a = PaneActivity::default();
        a.record_output();
        a.record_owner_input();
        assert_eq!(a.last_output_ms(), None);
        assert_eq!(a.last_owner_input_ms(), None);
    }
}

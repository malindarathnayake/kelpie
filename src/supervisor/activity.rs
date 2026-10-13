//! Fork-only per-pane activity stamps (Q1). Written lock-free by the PTY reader thread and
//! the headless input path; read by `agents.digest`. Stores are gated by `enabled()`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use crate::supervisor::fence::{FenceReason, SubmitFence};

/// One-shot operator handoff validity window. Expiry only reduces staleness; it is not
/// evidence of anything.
pub const HANDOFF_TTL_MS: u64 = 5 * 60 * 1000;

/// One-shot operator handoff armed from an attached client's keybinding: allows exactly one
/// accepted `agent.submit` to this pane despite the owner-input latch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handoff {
    pub armed_at_ms: u64,
    /// Registry event sequence at arming (orders it against other registry events).
    pub armed_seq: u64,
    /// Pane generation at arming; any later generation bump invalidates the handoff.
    pub generation: u64,
}

impl Handoff {
    /// Valid for the pane's current generation and not expired at `now_ms`.
    pub fn is_valid(&self, generation: u64, now_ms: u64) -> bool {
        self.generation == generation && now_ms.saturating_sub(self.armed_at_ms) < HANDOFF_TTL_MS
    }
}

/// Unix-ms stamps; 0 means "never observed". One instance per PTY runtime: a new runtime
/// starts with fresh stamps, an unlatched draft and no handoff.
#[derive(Debug, Default)]
pub struct PaneActivity {
    last_output_ms: AtomicU64,
    last_owner_input_ms: AtomicU64,
    /// Sticky owner-input latch: set by any owner input, never cleared in place. Output
    /// timestamps carry no correlation to the input echo and no native evidence shows the
    /// agent consumed or cleared typed-ahead bytes, so once set the draft stays Unknown for
    /// the rest of this PTY's lifetime.
    draft_latched: AtomicBool,
    /// Fast path for the owner-input hook: true while a handoff may be stored.
    handoff_present: AtomicBool,
    handoff: Mutex<Option<Handoff>>,
    /// Fast path for the owner-input hook: true while any submit fence is armed.
    fences_armed: AtomicBool,
    /// Fences of `agent.submit` writes in flight to this pane.
    submit_fences: Mutex<Vec<SubmitFence>>,
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

    /// Owner input forwarded to the pane (keys, text, paste, attached-terminal bytes). Same
    /// cost profile plus one Release store; sets the sticky draft latch and cancels any
    /// armed handoff (the lock is taken only while one may be stored).
    pub fn record_owner_input(&self) {
        if !crate::supervisor::enabled() {
            return;
        }
        self.last_owner_input_ms
            .store(crate::supervisor::now_ms(), Ordering::Relaxed);
        self.draft_latched.store(true, Ordering::Release);
        if self.handoff_present.load(Ordering::Acquire) {
            self.clear_handoff();
        }
    }

    /// True once any owner input reached this PTY.
    pub fn draft_latched(&self) -> bool {
        self.draft_latched.load(Ordering::Acquire)
    }

    /// Stores a one-shot handoff, replacing any previous one.
    pub fn arm_handoff(&self, handoff: Handoff) {
        let mut slot = self
            .handoff
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = Some(handoff);
        self.handoff_present.store(true, Ordering::Release);
    }

    /// The stored handoff when it is valid for `generation` at `now_ms`.
    pub fn valid_handoff(&self, generation: u64, now_ms: u64) -> Option<Handoff> {
        if !self.handoff_present.load(Ordering::Acquire) {
            return None;
        }
        let slot = self
            .handoff
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        slot.filter(|h| h.is_valid(generation, now_ms))
    }

    /// Consumes exactly `expected`. False when it was cancelled or replaced meanwhile.
    pub fn consume_handoff(&self, expected: Handoff) -> bool {
        let mut slot = self
            .handoff
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *slot != Some(expected) {
            return false;
        }
        *slot = None;
        self.handoff_present.store(false, Ordering::Release);
        true
    }

    fn clear_handoff(&self) {
        let mut slot = self
            .handoff
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = None;
        self.handoff_present.store(false, Ordering::Release);
    }

    /// Arms a submit fence so owner input to this pane trips it.
    pub fn arm_submit_fence(&self, fence: SubmitFence) {
        let mut fences = self
            .submit_fences
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        fences.retain(|f| !f.is_closed());
        fences.push(fence);
        self.fences_armed.store(true, Ordering::Release);
    }

    /// Owner keyboard input is about to reach the pane: trips every armed submit fence.
    /// One Acquire load when nothing is armed; not gated by `enabled()` because fences
    /// exist only when the supervisor surface is enabled.
    pub fn trip_submit_fences(&self) {
        if !self.fences_armed.load(Ordering::Acquire) {
            return;
        }
        let mut fences = self
            .submit_fences
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for fence in fences.drain(..) {
            fence.trip(FenceReason::OwnerInput);
        }
        self.fences_armed.store(false, Ordering::Release);
    }

    /// Test-only: sets both stamps directly (0 means never observed).
    #[cfg(test)]
    pub fn set_stamps_for_test(&self, output_ms: u64, owner_input_ms: u64) {
        self.last_output_ms.store(output_ms, Ordering::Relaxed);
        self.last_owner_input_ms
            .store(owner_input_ms, Ordering::Relaxed);
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
    fn owner_input_trips_armed_fences_once() {
        let a = PaneActivity::default();
        let fence = SubmitFence::new();
        a.arm_submit_fence(fence.clone());
        a.trip_submit_fences();
        assert!(!fence.try_commit());
        assert_eq!(fence.close().reason, Some(FenceReason::OwnerInput));

        let later = SubmitFence::new();
        a.trip_submit_fences();
        a.arm_submit_fence(later.clone());
        assert!(later.try_commit());
    }

    #[test]
    fn owner_input_latches_for_the_life_of_the_activity() {
        let _reset = Reset;
        set_enabled_for_test(Some(true));
        let a = PaneActivity::default();
        assert!(!a.draft_latched());
        a.record_output();
        assert!(!a.draft_latched(), "output never latches");
        a.record_owner_input();
        assert!(a.draft_latched());
        a.record_output();
        a.set_stamps_for_test(10_000, 1);
        assert!(a.draft_latched(), "no stamp or output clears the latch");
        assert!(
            !PaneActivity::default().draft_latched(),
            "a fresh PTY starts unlatched"
        );
    }

    #[test]
    fn handoff_is_one_shot_replaced_and_cancelled_by_owner_input() {
        let _reset = Reset;
        set_enabled_for_test(Some(true));
        let a = PaneActivity::default();
        let h = |seq| Handoff {
            armed_at_ms: 100,
            armed_seq: seq,
            generation: 1,
        };
        assert_eq!(a.valid_handoff(1, 100), None);
        a.arm_handoff(h(1));
        a.arm_handoff(h(2));
        assert_eq!(a.valid_handoff(1, 100), Some(h(2)), "re-arming replaces");
        assert_eq!(a.valid_handoff(2, 100), None, "other generation");
        assert!(!a.consume_handoff(h(1)), "stale handoff is not consumed");
        assert!(a.consume_handoff(h(2)));
        assert!(!a.consume_handoff(h(2)), "exactly once");
        assert_eq!(a.valid_handoff(1, 100), None);

        a.arm_handoff(h(3));
        a.record_owner_input();
        assert_eq!(a.valid_handoff(1, 100), None, "owner input cancels");
        assert!(!a.consume_handoff(h(3)));
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

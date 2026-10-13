//! Fork-only write-boundary fence for `agent.submit`. The PTY writer commits the fence
//! immediately before the first text byte of a fenced submission; a fence tripped before
//! that point makes the writer drop the whole submission (zero bytes). Agent-neutral and
//! independent of the PTY backend.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

/// Why a fence was tripped. Bounded set; also the receipt reason vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceReason {
    OwnerInput,
    GenerationChanged,
    PaneClosed,
}

const REASON_MASK: u8 = 0x0f;
const COMMITTED: u8 = 0x10;
const CLOSED: u8 = 0x20;

impl FenceReason {
    fn bits(self) -> u8 {
        match self {
            FenceReason::OwnerInput => 1,
            FenceReason::GenerationChanged => 2,
            FenceReason::PaneClosed => 3,
        }
    }

    fn from_bits(bits: u8) -> Option<Self> {
        match bits & REASON_MASK {
            1 => Some(FenceReason::OwnerInput),
            2 => Some(FenceReason::GenerationChanged),
            3 => Some(FenceReason::PaneClosed),
            _ => None,
        }
    }
}

/// Shared fence for one queued submission. Clones share state.
///
/// States: clear -> committed (writer is about to write the first text byte) or
/// clear -> tripped (writer will drop the submission). A trip after commit is recorded
/// as a late trip until the fence is closed; trips after close are ignored.
#[derive(Debug, Clone, Default)]
pub struct SubmitFence(Arc<AtomicU8>);

/// Outcome of a fence as seen once the write finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FenceSnapshot {
    pub committed: bool,
    pub reason: Option<FenceReason>,
}

impl SubmitFence {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the first trip reason. Returns true when this call set it.
    pub fn trip(&self, reason: FenceReason) -> bool {
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
                (v & (REASON_MASK | CLOSED) == 0).then_some(v | reason.bits())
            })
            .is_ok()
    }

    /// Writer side, immediately before the first text byte: commits the fence when it
    /// was not tripped. Returns false when the submission must be dropped.
    pub fn try_commit(&self) -> bool {
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
                (v & REASON_MASK == 0).then_some(v | COMMITTED)
            })
            .is_ok()
    }

    /// Stops recording trips (the write finished) and returns the final state.
    pub fn close(&self) -> FenceSnapshot {
        let v = self.0.fetch_or(CLOSED, Ordering::AcqRel);
        FenceSnapshot {
            committed: v & COMMITTED != 0,
            reason: FenceReason::from_bits(v),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.0.load(Ordering::Acquire) & CLOSED != 0
    }

    /// True once the writer committed (passed the boundary before the first text byte).
    pub fn is_committed(&self) -> bool {
        self.0.load(Ordering::Acquire) & COMMITTED != 0
    }

    /// True once any trip reason was recorded (before or after commit).
    pub fn is_tripped(&self) -> bool {
        self.0.load(Ordering::Acquire) & REASON_MASK != 0
    }
}

/// Marker carried by the completion error of a fenced (dropped) submission.
#[derive(Debug)]
pub struct SubmissionFenced;

impl std::fmt::Display for SubmissionFenced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("submission fenced before the first byte was written")
    }
}

impl std::error::Error for SubmissionFenced {}

/// Completion error for a submission dropped by its fence; zero bytes were written.
pub fn fenced_error() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Interrupted, SubmissionFenced)
}

/// True for the error produced by `fenced_error`.
pub fn is_fenced_error(err: &std::io::Error) -> bool {
    err.get_ref()
        .is_some_and(|inner| inner.is::<SubmissionFenced>())
}

/// Writer helper: true when the submission may write its first text byte.
pub fn admit(fence: Option<&SubmitFence>) -> bool {
    fence.is_none_or(SubmitFence::try_commit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untripped_fence_commits_and_reports_no_reason() {
        let f = SubmitFence::new();
        assert!(f.try_commit());
        assert_eq!(
            f.close(),
            FenceSnapshot {
                committed: true,
                reason: None
            }
        );
    }

    #[test]
    fn tripped_fence_refuses_commit_and_keeps_first_reason() {
        let f = SubmitFence::new();
        let writer = f.clone();
        assert!(f.trip(FenceReason::OwnerInput));
        assert!(!f.trip(FenceReason::GenerationChanged));
        assert!(!writer.try_commit());
        assert_eq!(
            f.close(),
            FenceSnapshot {
                committed: false,
                reason: Some(FenceReason::OwnerInput)
            }
        );
    }

    #[test]
    fn trip_after_commit_is_late_and_trip_after_close_is_ignored() {
        let f = SubmitFence::new();
        assert!(f.try_commit());
        assert!(f.trip(FenceReason::GenerationChanged));
        let snap = f.close();
        assert!(snap.committed);
        assert_eq!(snap.reason, Some(FenceReason::GenerationChanged));

        let g = SubmitFence::new();
        assert!(g.try_commit());
        g.close();
        assert!(!g.trip(FenceReason::OwnerInput));
        assert_eq!(g.close().reason, None);
    }

    #[test]
    fn committed_and_tripped_are_observable() {
        let f = SubmitFence::new();
        assert!(!f.is_committed() && !f.is_tripped());
        assert!(f.try_commit());
        assert!(f.is_committed() && !f.is_tripped());
        f.trip(FenceReason::OwnerInput);
        assert!(f.is_committed() && f.is_tripped());
        let g = SubmitFence::new();
        g.trip(FenceReason::PaneClosed);
        assert!(!g.is_committed() && g.is_tripped());
    }

    #[test]
    fn fenced_error_is_distinguishable() {
        assert!(is_fenced_error(&fenced_error()));
        assert!(!is_fenced_error(&std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "other"
        )));
        assert!(admit(None));
        let f = SubmitFence::new();
        f.trip(FenceReason::PaneClosed);
        assert!(!admit(Some(&f)));
    }
}

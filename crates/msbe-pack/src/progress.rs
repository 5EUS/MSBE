//! Progress reporting and cooperative cancellation for long pack operations.

use crate::PackError;

/// Receives progress from a running pack operation and says whether to stop.
///
/// Operations check [`Progress::cancelled`] between steps and before any profile or output is
/// committed, so a cancelled operation leaves no partial profile and no partial output.
pub trait Progress {
    /// Reports that `completed` of `total` steps are done.
    fn report(&self, completed: u64, total: u64, message: &str);

    /// Whether the caller asked the operation to stop.
    fn cancelled(&self) -> bool;
}

/// Progress that reports nothing and never cancels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Silent;

impl Progress for Silent {
    fn report(&self, _: u64, _: u64, _: &str) {}

    fn cancelled(&self) -> bool {
        false
    }
}

/// Fails with [`PackError::Cancelled`] once the caller has asked to stop.
pub(crate) fn checkpoint(progress: &dyn Progress) -> Result<(), PackError> {
    if progress.cancelled() {
        Err(PackError::Cancelled)
    } else {
        Ok(())
    }
}

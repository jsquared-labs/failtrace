mod shared;

pub mod cargo;
pub mod gotest;
pub mod jest;
pub mod pytest;
pub mod tsc;
pub mod vitest;
pub mod xcodebuild;

use crate::{schema::OutputDocument, storage::Sidecar};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AnalyzeOptions {
    pub max_failures: Option<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("unrecognized format: {reason}")]
    UnrecognizedFormat { reason: String },
    #[error("invalid sidecar: {reason}")]
    InvalidSidecar { reason: String },
    #[error("invariant violation: {0:?}")]
    InvariantViolation(Vec<crate::schema::Violation>),
}

pub(crate) trait AdapterImpl {
    fn analyze_log(
        log: &[u8],
        sidecar: &Sidecar,
        options: AnalyzeOptions,
    ) -> Result<OutputDocument, AdapterError>;
}

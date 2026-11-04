//! Typed Rust expression of the Failtrace v1.6 output schema.

mod hash;
mod invariants;
mod normalize;
mod types;

pub use hash::{
    FailureIdParts, derive_failure_id, derive_test_group_id, failure_id, strip_params_pytest,
    test_group_id,
};
pub use invariants::{Violation, final_outcome, validate};
pub use normalize::normalize_message;
pub use types::*;

//! Error types for the controller.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Kubernetes API error: {0}")]
    Kube(#[from] kube::Error),
    // Add project-specific errors as you implement decomposition logic, e.g.:
    // #[error("Decomposition precondition not met: {0}")]
    // DecompositionBlocked(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

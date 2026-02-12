//! Safe container decomposition controller — library root.
//!
//! Architecture:
//! - **Watcher** (controller) — watches Pods, Deployments, StatefulSets
//! - **Policy engine** — enforces ordering, traffic drain, readiness, state handover
//! - **Decommission FSM** — drives pod lifecycle during graceful shutdown

pub mod controller;
pub mod decommission;
pub mod error;
pub mod policy;

#[cfg(feature = "health")]
pub mod health;

#[cfg(feature = "metrics")]
pub mod metrics;

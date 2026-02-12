//! Decommission FSM: states and transitions for graceful pod shutdown.
//!
//! Drives lifecycle: Running → Draining → ReadinessLost → PreStopRunning →
//! (optional) StateHandover → DeletionAllowed.

use serde::{Deserialize, Serialize};

/// FSM state for a pod under decommissioning.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecommissionState {
    /// Pod is not terminating or not yet tracked.
    #[default]
    Unknown,
    /// Pod has deletion timestamp; we are draining traffic (e.g. wait for endpoints removal).
    Draining,
    /// Readiness is false; verifying endpoints removed / propagation.
    ReadinessLost,
    /// preStop hook is running (or we injected one and are waiting).
    PreStopRunning,
    /// Optional: waiting for state handover (annotation / external signal).
    StateHandover,
    /// Safe to allow deletion (we may remove our finalizer here).
    DeletionAllowed,
}

/// Input events that drive FSM transitions (from watcher / reconcile).
#[derive(Clone, Debug)]
pub enum DecommissionEvent {
    PodTerminating,
    ReadinessLost,
    EndpointsRemoved,
    PreStopStarted,
    PreStopFinished,
    StateHandoverComplete,
    DeletionAllowed,
}

/// Decommission FSM: given current state and event, returns next state (and optional side effect).
pub fn transition(
    current: DecommissionState,
    event: DecommissionEvent,
) -> DecommissionState {
    use DecommissionEvent as E;
    use DecommissionState as S;

    match (current, event) {
        (S::Unknown, E::PodTerminating) => S::Draining,
        (S::Draining, E::ReadinessLost) => S::ReadinessLost,
        (S::Draining, E::EndpointsRemoved) => S::ReadinessLost,
        (S::ReadinessLost, E::PreStopStarted) => S::PreStopRunning,
        (S::ReadinessLost, E::PreStopFinished) => S::DeletionAllowed,
        (S::PreStopRunning, E::PreStopFinished) => S::DeletionAllowed,
        (S::PreStopRunning, E::StateHandoverComplete) => S::StateHandover,
        (S::StateHandover, E::StateHandoverComplete) => S::DeletionAllowed,
        (S::StateHandover, E::DeletionAllowed) => S::DeletionAllowed,
        (state, _) => state,
    }
}

/// Persist FSM state per pod (e.g. in annotation or status).
/// Stored under custom annotation key, e.g. `decomposition.dat6.io/state`.
pub fn state_to_annotation_value(state: DecommissionState) -> String {
    match state {
        DecommissionState::Unknown => "unknown",
        DecommissionState::Draining => "draining",
        DecommissionState::ReadinessLost => "readiness_lost",
        DecommissionState::PreStopRunning => "pre_stop_running",
        DecommissionState::StateHandover => "state_handover",
        DecommissionState::DeletionAllowed => "deletion_allowed",
    }
    .to_string()
}

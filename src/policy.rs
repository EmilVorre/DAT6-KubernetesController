//! Policy engine: what the controller **enforces** during safe decomposition.
//!
//! - Graceful shutdown ordering
//! - Traffic draining before deletion
//! - Verification of readiness loss
//! - Optional state handover validation

use serde::{Deserialize, Serialize};

/// Configuration for how workloads are decommissioned.
/// Can be loaded from ConfigMap, CRD, or annotations.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DecommissionPolicy {
    /// Enforce ordering when scaling down (e.g. drain leader last, or by ordinal).
    pub graceful_shutdown_ordering: GracefulShutdownOrdering,
    /// Require traffic to be drained before allowing deletion.
    pub traffic_drain: TrafficDrainPolicy,
    /// Require verification that the pod is no longer ready (endpoints removed) before deletion.
    pub verify_readiness_loss: bool,
    /// **S1 — Early Readiness Removal:** When a pod is terminating, set custom readiness gate
    /// to False so the pod is removed from Service endpoints immediately (stops new traffic),
    /// then allow in-flight requests to drain.
    pub early_readiness_removal: bool,
    /// **S2 — Active Drain Verification:** poll pod /drainez before allowing deletion.
    pub drain_verification: bool,
    /// Optional: validate state handover (e.g. migration, backup) before deletion.
    pub state_handover: Option<StateHandoverConfig>,
}

/// How to order shutdown of multiple pods (e.g. StatefulSet ordinals, or by label).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GracefulShutdownOrdering {
    /// No ordering; any pod can be removed first.
    #[default]
    None,
    /// By StatefulSet-style ordinal (higher index first, or configurable).
    ByOrdinal { drain_high_first: bool },
    /// By custom label value (e.g. "role=leader" last).
    ByLabel {
        label_key: String,
        drain_last_values: Vec<String>,
    },
}

/// When to consider traffic drained (before proceeding to deletion).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TrafficDrainPolicy {
    /// No explicit drain; rely on kube-proxy/endpoints only.
    #[default]
    None,
    /// Wait until pod is removed from Service endpoints (readiness = false, then delay).
    WaitForEndpointsRemoval,
    /// Custom delay after readiness goes false (e.g. for propagation).
    DelayAfterReadinessLoss { seconds: u32 },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateHandoverConfig {
    /// e.g. "migration-complete" or "backup-verified"
    pub completion_annotation: String,
    /// Max time to wait for the annotation before allowing deletion anyway.
    pub timeout_seconds: u32,
}

/// Result of evaluating policy for a pod: what action to take next.
#[derive(Clone, Debug)]
pub enum PolicyDecision {
    /// No action; pod is not terminating or policy does not apply.
    NoAction,
    /// **S1:** Set custom readiness gate to False so pod is removed from Service endpoints.
    EnsureReadinessRemoved { requeue_after_secs: u64 },
    /// Delay deletion (e.g. traffic not yet drained).
    DelayDeletion {
        reason: String,
        requeue_after_secs: u64,
    },
    /// Inject or ensure preStop hook is present; then requeue.
    EnsurePreStop { requeue_after_secs: u64 },
    /// Allow deletion to proceed (remove finalizer if we added one, or do nothing).
    AllowDeletion,
    /// Wait for state handover (annotation / external signal).
    WaitForStateHandover { requeue_after_secs: u64 },
    /// **S2:** Poll pod /drainez and wait until ready_to_delete=true.
    WaitForDrainVerification,
}

/// Policy engine: given current pod/workload state and policy, returns the next decision.
pub struct PolicyEngine;

/// Condition type for S1 — Early Readiness Removal (must match pod spec readinessGates).
pub const READINESS_GATE_CONDITION_TYPE: &str = "decomposition.dat6.io/drain";

impl PolicyEngine {
    /// Evaluate what to do for this pod based on policy and current cluster state.
    ///
    /// Strategy semantics:
    ///
    /// - **S1 — Early Readiness Removal:** terminating pod has its readiness
    ///   gate flipped to `False` so it leaves Service endpoints before
    ///   preStop/grace. Once the gate is `False`, no further action.
    /// - **S2 — Active Drain Verification:** treats S1 as a *prerequisite step*
    ///   (pull traffic before draining) and then polls `/drainez` until the
    ///   app reports `ready_to_delete: true` before letting Kubernetes delete
    ///   the pod.
    ///
    /// S2 is a strict superset of S1. The two flags are independent inputs but
    /// the evaluation here makes the combined behavior explicit so we never
    /// rely on reconcile-loop fall-through ordering.
    ///
    /// `has_readiness_gate` and `gate_is_true` describe the
    /// `decomposition.dat6.io/drain` readiness gate specifically, *not* the
    /// pod's overall `Ready` condition. We use the gate state directly because
    /// on a pod without the gate (e.g. `baseline` overlay applied while the
    /// controller has S1 enabled) the overall `Ready` is True purely from the
    /// readiness probe — there is nothing the controller can flip — and using
    /// the overall condition would loop the FSM forever returning
    /// `EnsureReadinessRemoved`.
    pub fn evaluate(
        policy: &DecommissionPolicy,
        _pod_name: &str,
        is_terminating: bool,
        has_readiness_gate: bool,
        gate_is_true: bool,
        _fsm_state: &crate::decommission::DecommissionState,
    ) -> PolicyDecision {
        if !is_terminating {
            return PolicyDecision::NoAction;
        }

        let s1_or_s2 = policy.early_readiness_removal || policy.drain_verification;

        // Step 1 of S1/S2: ensure readiness gate flipped to False (only
        // possible if the pod actually has the gate in its spec).
        if s1_or_s2 && has_readiness_gate && gate_is_true {
            return PolicyDecision::EnsureReadinessRemoved {
                requeue_after_secs: 1,
            };
        }

        // Step 2 of S2: poll /drainez until ready_to_delete=true.
        if policy.drain_verification {
            return PolicyDecision::WaitForDrainVerification;
        }

        // S1 only (or S1 with no gate present) — nothing more to do; let
        // Kubernetes tear the pod down on its normal grace-period timeline.
        PolicyDecision::NoAction
    }
}

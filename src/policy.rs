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
    ByLabel { label_key: String, drain_last_values: Vec<String> },
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
    /// Delay deletion (e.g. traffic not yet drained).
    DelayDeletion { reason: String, requeue_after_secs: u64 },
    /// Inject or ensure preStop hook is present; then requeue.
    EnsurePreStop { requeue_after_secs: u64 },
    /// Allow deletion to proceed (remove finalizer if we added one, or do nothing).
    AllowDeletion,
    /// Wait for state handover (annotation / external signal).
    WaitForStateHandover { requeue_after_secs: u64 },
}

/// Policy engine: given current pod/workload state and policy, returns the next decision.
pub struct PolicyEngine;

impl PolicyEngine {
    /// Evaluate what to do for this pod based on policy and current cluster state.
    ///
    /// **Extension point:** Implement full logic using:
    /// - `policy` (from annotations, CRD, or global config)
    /// - Pod status (terminating, ready, conditions)
    /// - Optional: Service endpoints (traffic drain), StatefulSet/Deployment metadata (ordering)
    pub fn evaluate(
        _policy: &DecommissionPolicy,
        _pod_name: &str,
        _is_terminating: bool,
        _is_ready: bool,
        _fsm_state: &crate::decommission::DecommissionState,
    ) -> PolicyDecision {
        // TODO: implement full policy evaluation
        PolicyDecision::NoAction
    }
}

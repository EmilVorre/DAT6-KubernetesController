//! **Watcher** — Kubernetes controllers that watch Pods, Deployments, StatefulSets.
//!
//! Each reconcile loop uses the **Policy engine** and **Decommission FSM** to enforce
//! graceful shutdown ordering, traffic draining, readiness verification, and optional
//! state handover. Acts via preStop hooks, deletion delays, and custom annotations/CRDs.

use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::core::v1::Pod;
use kube::runtime::controller::{Action, Controller};
use kube::{Api, Client, ResourceExt};
use tracing::{info, instrument, warn};

use crate::decommission::DecommissionState;
use crate::error::{Error, Result};
use crate::policy::{DecommissionPolicy, PolicyDecision, PolicyEngine};

#[cfg(feature = "metrics")]
use crate::metrics::Metrics;

/// Shared context for all reconcilers (client, policy, optional metrics).
#[derive(Clone)]
pub struct ControllerContext {
    pub client: Client,
    /// Policy for graceful shutdown, traffic drain, readiness, state handover.
    pub policy: DecommissionPolicy,
    #[cfg(feature = "metrics")]
    pub metrics: Option<Arc<Metrics>>,
}

impl ControllerContext {
    pub fn new(client: Client, policy: DecommissionPolicy) -> Self {
        Self {
            client,
            policy,
            #[cfg(feature = "metrics")]
            metrics: None,
        }
    }

    #[cfg(feature = "metrics")]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }
}

// ---------- Pod controller ----------

pub fn pod_controller(pods: Api<Pod>) -> Controller<Pod> {
    Controller::new(pods, kube::runtime::watcher::Config::default())
}

#[instrument(skip(ctx), fields(pod = %obj.name_any(), namespace = %obj.namespace().unwrap_or_default()))]
pub async fn reconcile_pod(obj: Arc<Pod>, ctx: Arc<ControllerContext>) -> Result<Action> {
    #[cfg(feature = "metrics")]
    if let Some(ref m) = ctx.metrics {
        m.reconciliations.with_label_values(&["pod"]).inc();
    }

    let name = obj.name_any();
    let _ns = obj.namespace().unwrap_or_default();
    let phase = obj
        .status
        .as_ref()
        .and_then(|s| s.phase.as_deref())
        .unwrap_or("Unknown");
    let is_terminating = obj.metadata.deletion_timestamp.is_some();
    let is_ready = obj
        .status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .map(|c| c.iter().any(|cond| cond.type_ == "Ready" && cond.status == "True"))
        .unwrap_or(false);

    info!(
        phase = %phase,
        is_terminating = is_terminating,
        is_ready = is_ready,
        "reconciling pod"
    );

    // Current FSM state (in real impl: read from pod annotation or in-memory store).
    let fsm_state = read_fsm_state_from_pod(&obj);

    let decision = PolicyEngine::evaluate(
        &ctx.policy,
        &name,
        is_terminating,
        is_ready,
        &fsm_state,
    );

    match decision {
        PolicyDecision::NoAction => {}
        PolicyDecision::DelayDeletion { requeue_after_secs, .. } => {
            return Ok(Action::requeue(Duration::from_secs(requeue_after_secs)));
        }
        PolicyDecision::EnsurePreStop { requeue_after_secs } => {
            // TODO: patch pod to add/ensure preStop hook; then requeue
            return Ok(Action::requeue(Duration::from_secs(requeue_after_secs)));
        }
        PolicyDecision::AllowDeletion => {
            // TODO: remove our finalizer if we added one
        }
        PolicyDecision::WaitForStateHandover { requeue_after_secs } => {
            return Ok(Action::requeue(Duration::from_secs(requeue_after_secs)));
        }
    }

    // TODO: drive FSM transitions based on pod status (readiness, endpoints, preStop)
    // and persist state via annotation (e.g. decomposition.dat6.io/state).

    Ok(Action::requeue(Duration::from_secs(300)))
}

fn read_fsm_state_from_pod(pod: &Pod) -> DecommissionState {
    let ann = pod
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get("decomposition.dat6.io/state").map(String::as_str));
    match ann {
        Some("draining") => DecommissionState::Draining,
        Some("readiness_lost") => DecommissionState::ReadinessLost,
        Some("pre_stop_running") => DecommissionState::PreStopRunning,
        Some("state_handover") => DecommissionState::StateHandover,
        Some("deletion_allowed") => DecommissionState::DeletionAllowed,
        _ => DecommissionState::Unknown,
    }
}

pub fn error_policy_pod(
    object: Arc<Pod>,
    err: &Error,
    _ctx: Arc<ControllerContext>,
) -> Action {
    warn!(
        pod = %object.name_any(),
        error = %err,
        "pod reconcile failed, requeuing"
    );
    Action::requeue(Duration::from_secs(5))
}

// ---------- Deployment controller ----------

pub fn deployment_controller(deployments: Api<Deployment>) -> Controller<Deployment> {
    Controller::new(deployments, kube::runtime::watcher::Config::default())
}

#[instrument(skip(ctx), fields(deployment = %obj.name_any(), namespace = %obj.namespace().unwrap_or_default()))]
pub async fn reconcile_deployment(
    obj: Arc<Deployment>,
    ctx: Arc<ControllerContext>,
) -> Result<Action> {
    #[cfg(feature = "metrics")]
    if let Some(ref m) = ctx.metrics {
        m.reconciliations.with_label_values(&["deployment"]).inc();
    }

    let name = obj.name_any();
    info!("reconciling deployment (scale-down / lifecycle events)");

    // TODO: on scale-down, enforce ordering; trigger pod reconciliations for pods
    // that will be removed; optionally inject preStop or deletion delays via policy.

    let _ = (name, ctx);
    Ok(Action::requeue(Duration::from_secs(300)))
}

pub fn error_policy_deployment(
    object: Arc<Deployment>,
    err: &Error,
    _ctx: Arc<ControllerContext>,
) -> Action {
    warn!(
        deployment = %object.name_any(),
        error = %err,
        "deployment reconcile failed, requeuing"
    );
    Action::requeue(Duration::from_secs(5))
}

// ---------- StatefulSet controller ----------

pub fn statefulset_controller(statefulsets: Api<StatefulSet>) -> Controller<StatefulSet> {
    Controller::new(statefulsets, kube::runtime::watcher::Config::default())
}

#[instrument(skip(ctx), fields(statefulset = %obj.name_any(), namespace = %obj.namespace().unwrap_or_default()))]
pub async fn reconcile_statefulset(
    obj: Arc<StatefulSet>,
    ctx: Arc<ControllerContext>,
) -> Result<Action> {
    #[cfg(feature = "metrics")]
    if let Some(ref m) = ctx.metrics {
        m.reconciliations.with_label_values(&["statefulset"]).inc();
    }

    let name = obj.name_any();
    info!("reconciling statefulset (scale-down / termination ordering)");

    // TODO: use GracefulShutdownOrdering::ByOrdinal on scale-down; coordinate
    // with pod reconciler for ordered drain and preStop.

    let _ = (name, ctx);
    Ok(Action::requeue(Duration::from_secs(300)))
}

pub fn error_policy_statefulset(
    object: Arc<StatefulSet>,
    err: &Error,
    _ctx: Arc<ControllerContext>,
) -> Action {
    warn!(
        statefulset = %object.name_any(),
        error = %err,
        "statefulset reconcile failed, requeuing"
    );
    Action::requeue(Duration::from_secs(5))
}

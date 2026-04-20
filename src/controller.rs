//! **Watcher** — Kubernetes controllers that watch Pods, Deployments, StatefulSets.
//!
//! Each reconcile loop uses the **Policy engine** and **Decommission FSM** to enforce
//! graceful shutdown ordering, traffic draining, readiness verification, and optional
//! state handover. Acts via preStop hooks, deletion delays, and custom annotations/CRDs.

use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Patch, PatchParams};
use kube::runtime::controller::{Action, Controller};
use kube::{Api, Client, ResourceExt};
use reqwest::Client as HttpClient;
use serde::Deserialize;
use serde_json::json;
use tracing::{info, instrument, warn};

use crate::decommission::{transition, DecommissionEvent, DecommissionState};
use crate::error::{Error, Result};
use crate::policy::{
    DecommissionPolicy, PolicyDecision, PolicyEngine, READINESS_GATE_CONDITION_TYPE,
};

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
    let ns = obj.namespace().unwrap_or_default();
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
        .map(|c| {
            c.iter()
                .any(|cond| cond.type_ == "Ready" && cond.status == "True")
        })
        .unwrap_or(false);

    info!(
        phase = %phase,
        is_terminating = is_terminating,
        is_ready = is_ready,
        "reconciling pod"
    );

    // Current FSM state (in real impl: read from pod annotation or in-memory store).
    let mut fsm_state = read_fsm_state_from_pod(&obj);
    if is_terminating && fsm_state == DecommissionState::Unknown {
        fsm_state = transition(fsm_state, DecommissionEvent::PodTerminating);
        persist_fsm_state(&ctx.client, &ns, &name, fsm_state).await?;
    }

    let decision = PolicyEngine::evaluate(&ctx.policy, &name, is_terminating, is_ready, &fsm_state);

    // Keep pods schedulable/ready by default when S1/S2 readiness gates are present.
    // Readiness gates default to False unless explicitly set in pod status.
    if has_readiness_gate(&obj, READINESS_GATE_CONDITION_TYPE)
        && !is_terminating
        && !readiness_gate_is_true(&obj, READINESS_GATE_CONDITION_TYPE)
    {
        let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &ns);
        if let Err(e) = patch_pod_readiness_gate_status(
            &pods,
            &name,
            &obj,
            "True",
            "ReadyForService",
            "controller marks readiness gate true while pod is active",
        )
        .await
        {
            warn!(pod = %name, error = %e, "failed to set readiness gate True");
            return Ok(Action::requeue(Duration::from_secs(2)));
        }
    }

    match decision {
        PolicyDecision::NoAction => {}
        PolicyDecision::EnsureReadinessRemoved { requeue_after_secs } => {
            // S1 — Early Readiness Removal: patch pod status so our readiness gate is False.
            if has_readiness_gate(&obj, READINESS_GATE_CONDITION_TYPE) {
                let ns = obj.namespace().unwrap_or_default();
                let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &ns);
                if let Err(e) = patch_pod_readiness_gate_status(
                    &pods,
                    &name,
                    &obj,
                    "False",
                    "EarlyReadinessRemoval",
                    "S1: removed from Service endpoints before drain",
                )
                .await
                {
                    warn!(pod = %name, error = %e, "failed to patch pod readiness gate");
                    return Ok(Action::requeue(Duration::from_secs(2)));
                }
                info!(pod = %name, "set readiness gate to False (S1 early removal)");
            }
            return Ok(Action::requeue(Duration::from_secs(requeue_after_secs)));
        }
        PolicyDecision::WaitForDrainVerification => {
            let drained = poll_pod_drainez(&obj).await;
            if drained {
                let next_state = transition(fsm_state, DecommissionEvent::DrainVerified);
                persist_fsm_state(&ctx.client, &ns, &name, next_state).await?;
                remove_pod_finalizer(&ctx.client, &ns, &name).await?;
                info!(pod = %name, "drain verified; deletion allowed");
                return Ok(Action::requeue(Duration::from_secs(1)));
            }
            return Ok(Action::requeue(Duration::from_millis(500)));
        }
        PolicyDecision::DelayDeletion {
            requeue_after_secs, ..
        } => {
            return Ok(Action::requeue(Duration::from_secs(requeue_after_secs)));
        }
        PolicyDecision::EnsurePreStop { requeue_after_secs } => {
            // TODO: patch pod to add/ensure preStop hook; then requeue
            return Ok(Action::requeue(Duration::from_secs(requeue_after_secs)));
        }
        PolicyDecision::AllowDeletion => {
            remove_pod_finalizer(&ctx.client, &ns, &name).await?;
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

async fn persist_fsm_state(client: &Client, namespace: &str, pod_name: &str, state: DecommissionState) -> Result<()> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let patch = json!({
        "metadata": {
            "annotations": {
                "decomposition.dat6.io/state": crate::decommission::state_to_annotation_value(state),
            }
        }
    });
    let params = PatchParams::default();
    pods.patch(pod_name, &params, &Patch::Merge(&patch))
        .await
        .map_err(Error::from)?;
    Ok(())
}

#[derive(Deserialize)]
struct DrainezResponse {
    ready_to_delete: bool,
}

async fn poll_pod_drainez(pod: &Pod) -> bool {
    let Some(ip) = pod.status.as_ref().and_then(|s| s.pod_ip.as_ref()).cloned() else {
        // If pod IP already gone, treat as drained so we don't deadlock deletion.
        return true;
    };
    let http = HttpClient::new();
    let url = format!("http://{ip}:8080/drainez");
    for _ in 0..5 {
        let res = http
            .get(&url)
            .timeout(Duration::from_secs(1))
            .send()
            .await;
        match res {
            Ok(resp) => {
                if let Ok(payload) = resp.json::<DrainezResponse>().await {
                    if payload.ready_to_delete {
                        return true;
                    }
                }
            }
            Err(_) => {
                // Pod unreachable -> allow deletion to avoid deadlock.
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // Repeated failures should not block forever.
    true
}

async fn remove_pod_finalizer(client: &Client, namespace: &str, pod_name: &str) -> Result<()> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let pod = pods.get(pod_name).await.map_err(Error::from)?;
    let remaining: Vec<String> = pod
        .metadata
        .finalizers
        .unwrap_or_default()
        .into_iter()
        .filter(|f| f != "decomposition.dat6.io/finalizer")
        .collect();
    let patch = json!({ "metadata": { "finalizers": remaining } });
    let params = PatchParams::default();
    pods.patch(pod_name, &params, &Patch::Merge(&patch))
        .await
        .map_err(Error::from)?;
    Ok(())
}

/// True if the pod spec has a readinessGate for our condition type (S1).
fn has_readiness_gate(pod: &Pod, condition_type: &str) -> bool {
    pod.spec
        .as_ref()
        .and_then(|s| s.readiness_gates.as_ref())
        .map(|gates| gates.iter().any(|g| g.condition_type == condition_type))
        .unwrap_or(false)
}

fn readiness_gate_is_true(pod: &Pod, condition_type: &str) -> bool {
    pod.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .map(|conditions| {
            conditions
                .iter()
                .any(|cond| cond.type_ == condition_type && cond.status == "True")
        })
        .unwrap_or(false)
}

/// Patch pod status to set our readiness gate condition status.
async fn patch_pod_readiness_gate_status(
    pods: &Api<Pod>,
    name: &str,
    pod: &Pod,
    status: &str,
    reason: &str,
    message: &str,
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    let mut conditions: Vec<serde_json::Value> = pod
        .status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .map(|c| {
            c.iter()
                .filter(|cond| cond.type_ != READINESS_GATE_CONDITION_TYPE)
                .map(|cond| {
                    json!({
                        "type": cond.type_,
                        "status": cond.status,
                        "lastProbeTime": cond.last_probe_time,
                        "lastTransitionTime": cond.last_transition_time,
                        "reason": cond.reason,
                        "message": cond.message,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    conditions.push(json!({
        "type": READINESS_GATE_CONDITION_TYPE,
        "status": status,
        "lastTransitionTime": now,
        "reason": reason,
        "message": message,
    }));
    let patch = json!({ "status": { "conditions": conditions } });
    let params = PatchParams::default();
    pods.patch_status(name, &params, &Patch::Merge(patch))
        .await
        .map_err(Error::from)?;
    Ok(())
}

pub fn error_policy_pod(object: Arc<Pod>, err: &Error, _ctx: Arc<ControllerContext>) -> Action {
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

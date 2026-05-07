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

/// Finalizer key the controller adds to S2-managed pods so that pod resource
/// deletion is held until the controller has verified `/drainez`.
pub const POD_FINALIZER: &str = "decomposition.dat6.io/finalizer";

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
    let has_gate = has_readiness_gate(&obj, READINESS_GATE_CONDITION_TYPE);
    let gate_is_true_now = readiness_gate_is_true(&obj, READINESS_GATE_CONDITION_TYPE);

    info!(
        phase = %phase,
        is_terminating = is_terminating,
        is_ready = is_ready,
        has_gate = has_gate,
        gate_is_true = gate_is_true_now,
        "reconciling pod"
    );

    // S2 — Active Drain Verification: the controller must hold the pod resource
    // via a finalizer until `/drainez` reports `ready_to_delete`. The finalizer
    // *must* be added before the pod begins terminating; if we wait until we see
    // `deletionTimestamp`, the kubelet may already have started the termination
    // sequence and the pod may be removed before we can react. Add it as soon as
    // the pod is Running.
    if ctx.policy.drain_verification
        && !is_terminating
        && phase == "Running"
        && !has_pod_finalizer(&obj, POD_FINALIZER)
    {
        if let Err(e) = add_pod_finalizer(&ctx.client, &ns, &name, &obj).await {
            warn!(pod = %name, error = %e, "failed to add S2 finalizer; will retry");
            return Ok(Action::requeue(Duration::from_secs(2)));
        }
        info!(pod = %name, finalizer = %POD_FINALIZER, "added S2 finalizer");
        // requeue quickly so we observe the patched object next iteration
        return Ok(Action::requeue(Duration::from_secs(1)));
    }

    // Current FSM state (in real impl: read from pod annotation or in-memory store).
    let mut fsm_state = read_fsm_state_from_pod(&obj);
    if is_terminating && fsm_state == DecommissionState::Unknown {
        fsm_state = transition(fsm_state, DecommissionEvent::PodTerminating);
        persist_fsm_state(&ctx.client, &ns, &name, fsm_state).await?;

        // Best-effort fallback: if we somehow missed the Running window (e.g.
        // controller started after the pod was created), still try to add the
        // finalizer here. This is racy — Kubernetes may have already started
        // tearing the pod down — but it's strictly better than no-op.
        if ctx.policy.drain_verification && !has_pod_finalizer(&obj, POD_FINALIZER) {
            match add_pod_finalizer(&ctx.client, &ns, &name, &obj).await {
                Ok(_) => info!(
                    pod = %name,
                    "added S2 finalizer late (pod was already terminating)"
                ),
                Err(e) => warn!(
                    pod = %name,
                    error = %e,
                    "failed to add S2 finalizer late; deletion will not be gated"
                ),
            }
        }
    }

    let decision = PolicyEngine::evaluate(
        &ctx.policy,
        &name,
        is_terminating,
        has_gate,
        gate_is_true_now,
        &fsm_state,
    );

    // Keep pods schedulable/ready by default when S1/S2 readiness gates are present.
    // Readiness gates default to False unless explicitly set in pod status.
    if has_gate && !is_terminating && !gate_is_true_now {
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
            // The policy only returns this decision when `has_gate && gate_is_true`,
            // so the patch is always applicable here.
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
            return Ok(Action::requeue(Duration::from_secs(requeue_after_secs)));
        }
        PolicyDecision::WaitForDrainVerification => {
            let outcome = poll_pod_drainez(&obj).await;
            // How long has the pod been terminating? Use that to bound the
            // unreachable-fallback window so a permanently-dead pod doesn't
            // block deletion forever.
            let terminating_for = obj
                .metadata
                .deletion_timestamp
                .as_ref()
                .map(|t| {
                    chrono::Utc::now()
                        .signed_duration_since(t.0)
                        .num_seconds()
                        .max(0)
                })
                .unwrap_or(0);
            // Pod's grace period (clamped) gives us an upper bound on how long
            // the container could plausibly still be alive. After that, the
            // kubelet has SIGKILL'd it; we should release the finalizer.
            let grace = obj
                .spec
                .as_ref()
                .and_then(|s| s.termination_grace_period_seconds)
                .unwrap_or(30);

            match outcome {
                DrainezPollOutcome::ReadyToDelete => {
                    let next_state = transition(fsm_state, DecommissionEvent::DrainVerified);
                    persist_fsm_state(&ctx.client, &ns, &name, next_state).await?;
                    remove_pod_finalizer(&ctx.client, &ns, &name).await?;
                    info!(pod = %name, "drain verified; deletion allowed");
                    return Ok(Action::requeue(Duration::from_secs(1)));
                }
                DrainezPollOutcome::NotYet => {
                    return Ok(Action::requeue(Duration::from_millis(500)));
                }
                DrainezPollOutcome::Unreachable => {
                    if terminating_for >= i64::from(grace) {
                        warn!(
                            pod = %name,
                            terminating_for_s = terminating_for,
                            grace_s = grace,
                            "pod /drainez unreachable past grace period; removing finalizer to avoid deadlock"
                        );
                        let next_state = transition(fsm_state, DecommissionEvent::DrainVerified);
                        persist_fsm_state(&ctx.client, &ns, &name, next_state).await?;
                        remove_pod_finalizer(&ctx.client, &ns, &name).await?;
                        return Ok(Action::requeue(Duration::from_secs(1)));
                    }
                    // Pod might just be slow to come up its termination phase
                    // (or transiently unreachable). Keep waiting.
                    return Ok(Action::requeue(Duration::from_millis(500)));
                }
            }
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

/// Outcome of a single `/drainez` poll. We deliberately distinguish "still
/// has in-flight work" from "pod is unreachable (likely already dead)" so the
/// reconcile loop can wait or escalate appropriately. Returning `true` on any
/// error — as the previous implementation did — masks bugs (the pod can be
/// SIGKILL'd before we ever observe a successful poll, which makes S2's
/// deletion-gating effectively a no-op).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrainezPollOutcome {
    /// `/drainez` returned `ready_to_delete: true` — safe to remove finalizer.
    ReadyToDelete,
    /// `/drainez` responded but reports active in-flight work.
    NotYet,
    /// Pod IP missing or HTTP request failed — pod is most likely already gone.
    /// The reconcile loop should bound how long it waits for this state before
    /// giving up and removing the finalizer to avoid a deadlock.
    Unreachable,
}

async fn poll_pod_drainez(pod: &Pod) -> DrainezPollOutcome {
    let Some(ip) = pod.status.as_ref().and_then(|s| s.pod_ip.as_ref()).cloned() else {
        return DrainezPollOutcome::Unreachable;
    };
    let http = HttpClient::new();
    let url = format!("http://{ip}:8080/drainez");
    let res = http
        .get(&url)
        .timeout(Duration::from_secs(1))
        .send()
        .await;
    match res {
        Ok(resp) => match resp.json::<DrainezResponse>().await {
            Ok(payload) => {
                if payload.ready_to_delete {
                    DrainezPollOutcome::ReadyToDelete
                } else {
                    DrainezPollOutcome::NotYet
                }
            }
            Err(_) => DrainezPollOutcome::NotYet,
        },
        Err(_) => DrainezPollOutcome::Unreachable,
    }
}

async fn add_pod_finalizer(
    client: &Client,
    namespace: &str,
    pod_name: &str,
    pod: &Pod,
) -> Result<()> {
    let mut finalizers: Vec<String> = pod.metadata.finalizers.clone().unwrap_or_default();
    if finalizers.iter().any(|f| f == POD_FINALIZER) {
        return Ok(());
    }
    finalizers.push(POD_FINALIZER.to_string());
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let patch = json!({ "metadata": { "finalizers": finalizers } });
    let params = PatchParams::default();
    pods.patch(pod_name, &params, &Patch::Merge(&patch))
        .await
        .map_err(Error::from)?;
    Ok(())
}

fn has_pod_finalizer(pod: &Pod, key: &str) -> bool {
    pod.metadata
        .finalizers
        .as_ref()
        .map(|fs| fs.iter().any(|f| f == key))
        .unwrap_or(false)
}

async fn remove_pod_finalizer(client: &Client, namespace: &str, pod_name: &str) -> Result<()> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let pod = pods.get(pod_name).await.map_err(Error::from)?;
    let remaining: Vec<String> = pod
        .metadata
        .finalizers
        .unwrap_or_default()
        .into_iter()
        .filter(|f| f != POD_FINALIZER)
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

//! Safe container decomposition controller — binary entrypoint.
//!
//! **Watches:** Pods, Deployments, StatefulSets.
//! **Enforces:** graceful shutdown ordering, traffic draining, readiness verification,
//! optional state handover. **Acts via:** preStop hooks, deletion delays, annotations/CRDs.

use std::sync::Arc;

use futures::StreamExt;
use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use kube::Client;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use DAT6_KubernetesController::controller::{
    deployment_controller, error_policy_deployment, error_policy_pod, error_policy_statefulset,
    pod_controller, reconcile_deployment, reconcile_pod, reconcile_statefulset,
    statefulset_controller, ControllerContext,
};
use DAT6_KubernetesController::policy::DecommissionPolicy;

#[cfg(feature = "health")]
use DAT6_KubernetesController::health;

#[cfg(feature = "metrics")]
use DAT6_KubernetesController::metrics::Metrics;

#[tokio::main]
async fn main() -> Result<(), kube::Error> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let client = Client::try_default().await?;
    let policy = {
        let mut p = DecommissionPolicy::default();
        // S1 — Early Readiness Removal: set when using k8s/app/overlays/s1-early-readiness
        p.early_readiness_removal = std::env::var("DAT6_EARLY_READINESS_REMOVAL")
            .as_deref()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        p.drain_verification = std::env::var("DAT6_DRAIN_VERIFICATION")
            .as_deref()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if p.early_readiness_removal {
            info!("S1 — Early Readiness Removal enabled");
        }
        if p.drain_verification {
            info!("S2 — Active Drain Verification enabled");
        }
        p
    };

    let ctx = Arc::new({
        #[cfg(not(feature = "metrics"))]
        let c = ControllerContext::new(client.clone(), policy);
        #[cfg(feature = "metrics")]
        let c = {
            let mut c = ControllerContext::new(client.clone(), policy);
            if let Ok(m) = Metrics::new() {
                c = c.with_metrics(Arc::new(m));
            }
            c
        };
        c
    });

    #[cfg(feature = "health")]
    let _server = {
        let addr: std::net::SocketAddr = ([0, 0, 0, 0], 8080).into();
        health::spawn_server(addr);
    };

    let pods: Api<Pod> = Api::namespaced(client.clone(), "default");
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), "default");
    let statefulsets: Api<StatefulSet> = Api::namespaced(client.clone(), "default");

    info!("starting watchers: Pods, Deployments, StatefulSets");

    let pod_fut = pod_controller(pods)
        .run(reconcile_pod, error_policy_pod, ctx.clone())
        .for_each(|res| async move {
            if let Err(e) = res {
                tracing::error!(error = %e, "pod reconciliation failed");
            }
        });
    let dep_fut = deployment_controller(deployments)
        .run(reconcile_deployment, error_policy_deployment, ctx.clone())
        .for_each(|res| async move {
            if let Err(e) = res {
                tracing::error!(error = %e, "deployment reconciliation failed");
            }
        });
    let ss_fut = statefulset_controller(statefulsets)
        .run(reconcile_statefulset, error_policy_statefulset, ctx.clone())
        .for_each(|res| async move {
            if let Err(e) = res {
                tracing::error!(error = %e, "statefulset reconciliation failed");
            }
        });

    tokio::select! {
        _ = pod_fut => { tracing::error!("pod controller exited"); }
        _ = dep_fut => { tracing::error!("deployment controller exited"); }
        _ = ss_fut => { tracing::error!("statefulset controller exited"); }
    }

    Ok(())
}

//! Optional Prometheus metrics (enable with `metrics` feature).
//!
//! Register and expose metrics for reconciliation, FSM state, policy decisions.

use prometheus::{register_int_counter_vec, register_int_gauge_vec, IntCounterVec, IntGaugeVec};

/// Application metrics. Create once at startup and pass via context.
pub struct Metrics {
    pub reconciliations: IntCounterVec,
    pub decommission_state: IntGaugeVec,
}

impl Metrics {
    /// Register metrics with the default registry. Call once at startup.
    pub fn new() -> Result<Self, prometheus::Error> {
        let reconciliations = register_int_counter_vec!(
            "decomposition_reconciliations_total",
            "Total reconcile runs by resource",
            &["resource"]
        )?;
        let decommission_state = register_int_gauge_vec!(
            "decomposition_fsm_state",
            "Decommission FSM state (1 = in state)",
            &["pod", "namespace", "state"]
        )?;
        Ok(Self {
            reconciliations,
            decommission_state,
        })
    }
}

//! CRDs, group neuroscience.flyops.io/v1alpha1.
//!
//! - `FlyBrain` (cluster-scoped): the operator creates one, named `flyops`. Its only knob is the switch.
//! - `FlyDecision` (namespaced, in the operator's namespace): one audit record per action the fly took.
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The FlyBrain the operator creates and drives. `oc flyops on|off` flips its switch.
pub const BRAIN_NAME: &str = "flyops";

#[derive(CustomResource, Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "neuroscience.flyops.io", version = "v1alpha1", kind = "FlyBrain",
    status = "FlyBrainStatus", shortname = "fb",
    printcolumn = r#"{"name":"Switch","type":"string","jsonPath":".status.switch"}"#,
    printcolumn = r#"{"name":"Last Response","type":"string","jsonPath":".status.lastResponse"}"#,
    printcolumn = r#"{"name":"Last Action","type":"string","jsonPath":".status.lastAction"}"#,
    printcolumn = r#"{"name":"Last Simulation","type":"date","jsonPath":".status.lastSimulationTime"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct FlyBrainSpec {
    /// The switch. false (default): the fly watches and thinks, and changes nothing.
    /// true: the fly may delete broken pods, restart broken workloads and drain broken nodes anywhere in the cluster.
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlyBrainStatus {
    /// ON or OFF, as the operator last applied it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub switch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_simulation_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_response: Option<String>,
    /// What the fly last did to the cluster.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_action: Option<String>,
    /// Set when the brain cannot run (e.g. the connectome failed to load).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

pub fn switch_label(enabled: bool) -> &'static str {
    if enabled { "ON" } else { "OFF" }
}

#[derive(CustomResource, Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "neuroscience.flyops.io", version = "v1alpha1", kind = "FlyDecision", namespaced,
    shortname = "fd",
    printcolumn = r#"{"name":"Action","type":"string","jsonPath":".spec.action"}"#,
    printcolumn = r#"{"name":"Result","type":"string","jsonPath":".spec.result"}"#,
    printcolumn = r#"{"name":"Time","type":"date","jsonPath":".spec.timestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct FlyDecisionSpec {
    pub timestamp: String,
    /// delete-pod | restart-workload | drain-node
    pub action: String,
    /// What was done, to what, or why nothing was.
    pub result: String,
    /// Strongest readout populations at the time (name, activity 0..1).
    #[serde(default)]
    pub neural_activity: Vec<PopulationSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct PopulationSummary {
    pub population: String,
    pub activity: f32,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn switch_is_off_by_default() {
        let s: FlyBrainSpec = serde_json::from_str("{}").unwrap();
        assert!(!s.enabled);
        assert_eq!(switch_label(s.enabled), "OFF");
        assert_eq!(switch_label(true), "ON");
    }
}

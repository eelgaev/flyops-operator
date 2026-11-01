//! Backend-agnostic brain types. The brain never sees Kubernetes objects or credentials.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SensoryStimulus {
    pub channel: String,
    pub intensity: f32,
    pub duration_ms: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SensoryFrame {
    pub observed_at: chrono::DateTime<chrono::Utc>,
    pub encoder_version: String,
    pub stimuli: Vec<SensoryStimulus>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PopulationActivity {
    pub population: String,
    pub activity: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NeuralOutput {
    pub simulated_ms: u32,
    pub populations: Vec<PopulationActivity>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BrainMetadata {
    pub backend: String,
    pub model: String,
    pub version: String,
}

/// Intentionally small: a neural output can never build arbitrary API requests. The brain only picks *which*
/// response; the operator picks the target from what is unhealthy in the cluster.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum CandidateResponse {
    NoOp,
    /// Publish a Kubernetes Event. Never changes anything.
    RaiseAlert { reason: String },
    /// Delete the worst unhealthy, controller-owned pod (its controller replaces it).
    DeletePod,
    /// Rollout-restart the Deployment, StatefulSet or DaemonSet with the most unhealthy pods.
    RestartWorkload,
    /// Cordon a node that is down or under pressure and evict its pods.
    DrainNode,
}

impl CandidateResponse {
    pub fn name(&self) -> &'static str {
        match self {
            Self::NoOp => "no-op",
            Self::RaiseAlert { .. } => "raise-alert",
            Self::DeletePod => "delete-pod",
            Self::RestartWorkload => "restart-workload",
            Self::DrainNode => "drain-node",
        }
    }
    /// Changes the cluster (only ever when the switch is ON).
    pub fn is_mutating(&self) -> bool {
        matches!(self, Self::DeletePod | Self::RestartWorkload | Self::DrainNode)
    }
}

#[async_trait::async_trait]
pub trait BrainBackend: Send + Sync {
    fn metadata(&self) -> BrainMetadata;
    async fn reset(&self, seed: u64) -> anyhow::Result<()>;
    async fn stimulate(&self, input: SensoryFrame, duration_ms: u32) -> anyhow::Result<NeuralOutput>;
}

/// Names of the populations exposed by the built-in mock/visualisation. Project-defined, not anatomy.
pub const MOCK_POPULATIONS: &[&str] = &[
    "example-descending-population",
    "example-mushroom-body-population",
    "example-central-complex-population",
    "example-antennal-lobe-population",
    "example-optic-lobe-population",
];

// ------------------------------------------------------------------ mock backend

const WEIGHTS: &[(&str, &[(&str, f32)])] = &[
    ("example-descending-population", &[("looming", 1.0), ("abrupt", 0.8), ("strong-looming", 1.2), ("temperature", 0.3)]),
    ("example-mushroom-body-population", &[("odor-left", 0.9), ("odor-right", 0.7), ("touch", 0.2)]),
    ("example-central-complex-population", &[("touch", 0.8), ("temperature", 0.4), ("looming", 0.2)]),
    ("example-antennal-lobe-population", &[("odor-left", 0.6), ("odor-right", 0.6)]),
    ("example-optic-lobe-population", &[("looming", 0.5), ("strong-looming", 0.7), ("abrupt", 0.4)]),
];

#[derive(Default)]
pub struct MockBrainBackend {
    seed: std::sync::atomic::AtomicU64,
}

impl MockBrainBackend {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl BrainBackend for MockBrainBackend {
    fn metadata(&self) -> BrainMetadata {
        BrainMetadata { backend: "mock".into(), model: "linear-mock".into(), version: "1".into() }
    }
    async fn reset(&self, seed: u64) -> anyhow::Result<()> {
        self.seed.store(seed, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
    async fn stimulate(&self, input: SensoryFrame, duration_ms: u32) -> anyhow::Result<NeuralOutput> {
        let populations = WEIGHTS
            .iter()
            .map(|(pop, ws)| {
                let drive: f32 = ws
                    .iter()
                    .map(|(ch, w)| {
                        input.stimuli.iter().filter(|s| s.channel == *ch).map(|s| s.intensity * w).sum::<f32>()
                    })
                    .sum();
                // saturating response; small deterministic seed-dependent baseline
                let base = ((self.seed.load(std::sync::atomic::Ordering::Relaxed) % 7) as f32) * 0.002;
                PopulationActivity { population: (*pop).into(), activity: (1.0 - (-2.0 * drive).exp() + base).clamp(0.0, 1.0) }
            })
            .collect();
        Ok(NeuralOutput { simulated_ms: duration_ms, populations })
    }
}


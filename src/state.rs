//! Shared, bounded in-memory state for the dashboard and metrics.
use crate::observer::ObservedCounts;
use prometheus::{HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGaugeVec, Opts, Registry};
use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};
use std::sync::RwLock;

#[derive(Clone, Serialize, Default)]
pub struct DecisionView {
    pub time: String,
    pub response: String,
    pub detail: String,
    pub safety: String,
    pub outcome: String,
}

#[derive(Clone, Serialize, Default)]
pub struct BrainView {
    pub name: String,
    /// ON or OFF
    pub switch: String,
    pub backend: String,
    pub model: String,
    pub demo: bool,
    pub updated_at: String,
    pub ticks: u64,
    pub signals: BTreeMap<String, f32>,
    pub stimuli: Vec<(String, f32)>,
    pub populations: Vec<(String, f32)>,
    pub response: String,
    pub safety: String,
    pub error: Option<String>,
    pub counts: ObservedCounts,
    pub history: VecDeque<DecisionView>,
}

pub struct Metrics {
    pub registry: Registry,
    pub observed: IntGaugeVec,
    pub signals: prometheus::GaugeVec,
    pub sim_seconds: HistogramVec,
    pub ticks: IntCounter,
    pub activity: prometheus::GaugeVec,
    pub decisions: IntCounterVec,
    pub backend_errors: IntCounter,
    pub safety_blocks: IntCounterVec,
}

impl Metrics {
    pub fn new() -> Self {
        let r = Registry::new();
        let observed = IntGaugeVec::new(Opts::new("flyops_observed_objects", "Objects in observer caches"), &["kind"]).unwrap();
        let signals = prometheus::GaugeVec::new(Opts::new("flyops_cluster_signals", "Normalised cluster signals"), &["signal"]).unwrap();
        let sim_seconds = HistogramVec::new(HistogramOpts::new("flyops_simulation_duration_seconds", "Simulation cycle wall time"), &["backend"]).unwrap();
        let ticks = IntCounter::new("flyops_simulation_ticks_total", "Simulation ticks").unwrap();
        let activity = prometheus::GaugeVec::new(Opts::new("flyops_neural_activity", "Population activity"), &["population"]).unwrap();
        let decisions = IntCounterVec::new(Opts::new("flyops_decisions_total", "Decisions"), &["response", "safety_result"]).unwrap();
        let backend_errors = IntCounter::new("flyops_backend_errors_total", "Brain backend errors").unwrap();
        let safety_blocks = IntCounterVec::new(Opts::new("flyops_safety_blocks_total", "Safety blocks"), &["reason"]).unwrap();
        r.register(Box::new(observed.clone())).unwrap();
        r.register(Box::new(signals.clone())).unwrap();
        r.register(Box::new(sim_seconds.clone())).unwrap();
        r.register(Box::new(ticks.clone())).unwrap();
        r.register(Box::new(activity.clone())).unwrap();
        r.register(Box::new(decisions.clone())).unwrap();
        r.register(Box::new(backend_errors.clone())).unwrap();
        r.register(Box::new(safety_blocks.clone())).unwrap();
        Self { registry: r, observed, signals, sim_seconds, ticks, activity, decisions, backend_errors, safety_blocks }
    }
}

pub struct AppState {
    pub brains: RwLock<BTreeMap<String, BrainView>>,
    pub metrics: Metrics,
}

impl AppState {
    pub fn new() -> Self {
        Self { brains: RwLock::new(BTreeMap::new()), metrics: Metrics::new() }
    }
    pub fn publish(&self, v: BrainView) {
        self.brains.write().unwrap().insert(v.name.clone(), v);
    }
}

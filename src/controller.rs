//! The operator loop. Every tick: read the switch, sense the cluster, run the brain, maybe act.
use crate::api::{switch_label, FlyBrain, FlyBrainSpec, FlyBrainStatus, FlyDecision, FlyDecisionSpec, PopulationSummary, BRAIN_NAME};
use crate::brain::{BrainBackend, CandidateResponse, NeuralOutput, SensoryFrame};
use crate::decoder::OutputMapping;
use crate::encoding::{Hysteresis, InputMapping, Signals};
use crate::observer::Observer;
use crate::safety::{Governor, Verdict};
use crate::state::{AppState, BrainView, DecisionView};
use kube::{
    api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams},
    runtime::events::{Event, EventType, Recorder},
    Client, Resource, ResourceExt,
};
use std::{collections::VecDeque, sync::Arc, time::{Duration, Instant}};

pub const TICK: Duration = Duration::from_secs(5);
pub const WINDOW_MS: u32 = 250;
const SIM_TIMEOUT: Duration = Duration::from_secs(5);
const RETAIN_DECISIONS: usize = 100;

// ------------------------------------------------------------------ wiring

/// Cluster signal -> sensory channel (FlyWire neurons stimulated, see data/flywire-783/mapping.json) and gain.
pub const INPUTS: &[(&str, &str, f32)] = &[
    ("node_not_ready", "looming", 3.0),        // a node going dark looms -> escape neurons
    ("node_memory_pressure", "looming", 3.0),  // so does a node in trouble
    ("pod_crash_loop", "odor-left", 1.0),      // broken pods smell -> olfactory / mushroom-body circuit
    ("pod_pending_duration", "odor-right", 1.0),
    ("deployment_degraded", "abrupt", 3.0),    // degraded workloads -> ocellar / optic-lobe pathway
    ("job_failed", "touch", 1.0),
];

/// Readout population -> response and the activity (0..1) at which it fires, in PRIORITY order: the first one that
/// fires wins. Escape (a node in trouble) preempts everything, then fixing workloads, then single pods.
pub const OUTPUTS: &[(&str, &str, f32)] = &[
    ("example-descending-population", "drain-node", 0.2),
    ("example-optic-lobe-population", "restart-workload", 0.4),
    ("example-antennal-lobe-population", "delete-pod", 0.6),
    ("example-mushroom-body-population", "delete-pod", 0.6),
    ("example-central-complex-population", "raise-alert", 0.5),
];

// ------------------------------------------------------------------ one cycle (pure; shared with --demo)

pub struct CycleResult {
    pub frame: SensoryFrame,
    pub output: Option<NeuralOutput>,
    pub candidate: CandidateResponse,
    pub verdict: Verdict,
    pub error: Option<String>,
}

pub async fn run_cycle(backend: &dyn BrainBackend, governor: &mut Governor, signals: &Signals, switch_on: bool) -> CycleResult {
    let inputs: Vec<InputMapping> = INPUTS.iter().map(|(s, c, g)| InputMapping { signal: s.to_string(), channel: c.to_string(), gain: *g }).collect();
    let frame = crate::encoding::encode(signals, &inputs, chrono::Utc::now(), WINDOW_MS, 64);
    let mut error = None;
    let output = match tokio::time::timeout(SIM_TIMEOUT, backend.stimulate(frame.clone(), WINDOW_MS)).await {
        Ok(Ok(o)) => Some(o),
        Ok(Err(e)) => { error = Some(format!("brain error: {e}")); None }
        Err(_) => { error = Some("brain timeout".into()); None }
    };
    let mappings: Vec<OutputMapping> = OUTPUTS.iter().map(|(p, r, t)| OutputMapping { population: p.to_string(), response: r.to_string(), threshold: *t }).collect();
    let candidate = match &output {
        Some(o) => crate::decoder::decode(o, &mappings).unwrap_or_else(|e| { error = Some(format!("{e:?}")); CandidateResponse::NoOp }),
        None => CandidateResponse::NoOp,
    };
    let verdict = governor.evaluate(&candidate, switch_on, error.is_some(), Instant::now());
    CycleResult { frame, output, candidate, verdict, error }
}

// ------------------------------------------------------------------ the loop

pub struct Ctx {
    pub client: Client,
    pub observer: Arc<Observer>,
    pub state: Arc<AppState>,
    pub recorder: Recorder,
    /// FlyOps' own namespace: never touched by the fly, home of its FlyDecision records.
    pub own_ns: String,
    pub backend: Arc<dyn BrainBackend>,
    /// Set when the connectome could not be loaded and the mock stands in.
    pub backend_note: Option<String>,
}

pub async fn run(ctx: Ctx) {
    let brains: Api<FlyBrain> = Api::all(ctx.client.clone());
    let decisions: Api<FlyDecision> = Api::namespaced(ctx.client.clone(), &ctx.own_ns);
    let meta = ctx.backend.metadata();
    let mut governor = Governor::default();
    let mut hyst = Hysteresis::default();
    let mut history: VecDeque<DecisionView> = VecDeque::new();
    let (mut ticks, mut last_switch, mut last_response) = (0u64, None::<bool>, String::new());
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tracing::info!(backend = %meta.backend, model = %meta.model, version = %meta.version, "fly brain running");

    loop {
        interval.tick().await;
        let brain = match brains.get_opt(BRAIN_NAME).await {
            Ok(Some(b)) => b,
            Ok(None) => {
                // The switch lives on this object; recreate it (OFF) if someone deleted it.
                let b = FlyBrain::new(BRAIN_NAME, FlyBrainSpec::default());
                if let Err(e) = brains.create(&PostParams::default(), &b).await { tracing::warn!(error = %e, "creating FlyBrain failed"); }
                continue;
            }
            Err(e) => { tracing::warn!(error = %e, "reading FlyBrain failed"); continue; }
        };
        let on = brain.spec.enabled;
        if last_switch != Some(on) {
            let (ty, reason, note) = if on { (EventType::Warning, "SwitchedOn", "The fly now controls the cluster") }
                                     else { (EventType::Normal, "SwitchedOff", "The fly watches only and changes nothing") };
            publish(&ctx, &brain, ty, reason, note).await;
            tracing::info!(switch = switch_label(on), "switch");
            last_switch = Some(on);
        }

        let started = Instant::now();
        let signals = hyst.apply(&ctx.observer.signals(chrono::Utc::now()));
        let r = run_cycle(ctx.backend.as_ref(), &mut governor, &signals, on).await;
        let resp = r.candidate.name().to_string();
        let mut action_result = None;
        if r.verdict == Verdict::Allowed {
            if r.candidate.is_mutating() {
                let result = act(&ctx, &r.candidate).await;
                publish(&ctx, &brain, EventType::Warning, "FlyAction", &format!("{resp}: {result}")).await;
                record(&decisions, &resp, &result, &r).await;
                tracing::info!(action = %resp, %result, "fly acted");
                action_result = Some(result);
            } else if let CandidateResponse::RaiseAlert { reason } = &r.candidate {
                publish(&ctx, &brain, EventType::Warning, "FlyAlert", reason).await;
            }
        }

        ticks += 1;
        let m = &ctx.state.metrics;
        m.ticks.inc();
        m.sim_seconds.with_label_values(&[&meta.backend]).observe(started.elapsed().as_secs_f64());
        if r.error.is_some() { m.backend_errors.inc(); }
        m.decisions.with_label_values(&[resp.as_str(), if r.verdict == Verdict::Allowed { "allowed" } else { "blocked" }]).inc();
        if let Verdict::Blocked(why) = &r.verdict { m.safety_blocks.with_label_values(&[why]).inc(); }
        let counts = ctx.observer.counts();
        for (k, v) in [("node", counts.nodes), ("pod", counts.pods), ("deployment", counts.deployments), ("job", counts.jobs)] {
            m.observed.with_label_values(&[k]).set(v as i64);
        }
        for (k, v) in &signals { m.signals.with_label_values(&[k]).set(*v as f64); }
        if let Some(o) = &r.output { for p in &o.populations { m.activity.with_label_values(&[&p.population]).set(p.activity as f64); } }

        let now = chrono::Utc::now().to_rfc3339();
        if resp != "no-op" || resp != last_response {
            let detail = match &r.candidate { CandidateResponse::RaiseAlert { reason } => reason.clone(), _ => String::new() };
            history.push_front(DecisionView { time: now.clone(), response: resp.clone(), detail, safety: r.verdict.label(), outcome: action_result.clone().unwrap_or_default() });
            history.truncate(30);
        }
        last_response = resp.clone();
        ctx.state.publish(BrainView {
            name: BRAIN_NAME.into(), switch: switch_label(on).into(), backend: meta.backend.clone(), model: format!("{} {}", meta.model, meta.version),
            demo: false, updated_at: now.clone(), ticks, signals: signals.clone(),
            stimuli: r.frame.stimuli.iter().map(|s| (s.channel.clone(), s.intensity)).collect(),
            populations: r.output.as_ref().map(|o| o.populations.iter().map(|p| (p.population.clone(), p.activity)).collect()).unwrap_or_default(),
            response: resp.clone(), safety: r.verdict.label(), error: r.error.clone().or(ctx.backend_note.clone()), counts, history: history.clone(),
        });
        let status = FlyBrainStatus {
            switch: Some(switch_label(on).into()), last_simulation_time: Some(now), last_response: Some(resp),
            last_action: action_result, message: r.error.clone().or(ctx.backend_note.clone()),
        };
        let patch = serde_json::json!({"apiVersion": "neuroscience.flyops.io/v1alpha1", "kind": "FlyBrain", "status": status});
        if let Err(e) = brains.patch_status(BRAIN_NAME, &PatchParams::default(), &Patch::Merge(&patch)).await { tracing::warn!(error = %e, "status patch failed"); }
    }
}

async fn act(ctx: &Ctx, c: &CandidateResponse) -> String {
    let res = match c {
        CandidateResponse::DeletePod => crate::executor::delete_pod(ctx.client.clone(), &ctx.observer.pods(), &ctx.own_ns).await,
        CandidateResponse::RestartWorkload => crate::executor::restart_workload(ctx.client.clone(), &ctx.observer.pods(), &ctx.own_ns).await,
        CandidateResponse::DrainNode => crate::executor::drain_node(ctx.client.clone(), &ctx.observer.nodes(), &ctx.own_ns).await,
        _ => return "not an action".into(),
    };
    res.unwrap_or_else(|e| format!("failed: {e}"))
}

async fn publish(ctx: &Ctx, brain: &FlyBrain, type_: EventType, reason: &str, note: &str) {
    let ev = Event { type_, reason: reason.into(), note: Some(note.chars().take(1000).collect()), action: reason.into(), secondary: None };
    if let Err(e) = ctx.recorder.publish(&ev, &brain.object_ref(&())).await { tracing::warn!(error = %e, "event publish failed"); }
}

async fn record(api: &Api<FlyDecision>, action: &str, result: &str, r: &CycleResult) {
    let mut pops: Vec<PopulationSummary> = r.output.iter().flat_map(|o| &o.populations)
        .map(|p| PopulationSummary { population: p.population.clone(), activity: p.activity }).collect();
    pops.sort_by(|a, b| b.activity.total_cmp(&a.activity));
    let mut d = FlyDecision::new("", FlyDecisionSpec { timestamp: chrono::Utc::now().to_rfc3339(), action: action.into(), result: result.into(), neural_activity: pops });
    d.metadata.generate_name = Some(format!("{action}-"));
    d.metadata.name = None;
    if let Err(e) = api.create(&PostParams::default(), &d).await { tracing::warn!(error = %e, "FlyDecision create failed"); return; }
    if let Ok(list) = api.list(&ListParams::default()).await {
        let mut items = list.items;
        if items.len() > RETAIN_DECISIONS {
            items.sort_by(|a, b| a.spec.timestamp.cmp(&b.spec.timestamp));
            for old in items.iter().take(items.len() - RETAIN_DECISIONS) { let _ = api.delete(&old.name_any(), &DeleteParams::default()).await; }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brain::MockBrainBackend;

    fn sig(pairs: &[(&str, f32)]) -> Signals { pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect() }

    #[tokio::test]
    async fn quiet_cluster_is_noop() {
        let r = run_cycle(&MockBrainBackend::new(), &mut Governor::default(), &sig(&[("node_not_ready", 0.0)]), true).await;
        assert_eq!(r.candidate, CandidateResponse::NoOp);
    }
    #[tokio::test]
    async fn off_never_acts_on_acts() {
        let s = sig(&[("node_not_ready", 1.0)]);
        let off = run_cycle(&MockBrainBackend::new(), &mut Governor::default(), &s, false).await;
        assert!(off.candidate.is_mutating(), "the fly still decides: {:?}", off.candidate);
        assert_eq!(off.verdict, Verdict::Blocked("switch-off".into()));
        let on = run_cycle(&MockBrainBackend::new(), &mut Governor::default(), &s, true).await;
        assert_eq!(on.verdict, Verdict::Allowed);
    }
    #[tokio::test]
    async fn brain_failure_fails_closed() {
        struct Broken;
        #[async_trait::async_trait]
        impl BrainBackend for Broken {
            fn metadata(&self) -> crate::brain::BrainMetadata { crate::brain::BrainMetadata { backend: "x".into(), model: "x".into(), version: "x".into() } }
            async fn reset(&self, _: u64) -> anyhow::Result<()> { Ok(()) }
            async fn stimulate(&self, _: SensoryFrame, _: u32) -> anyhow::Result<NeuralOutput> { anyhow::bail!("boom") }
        }
        let r = run_cycle(&Broken, &mut Governor::default(), &sig(&[("node_not_ready", 1.0)]), true).await;
        assert_eq!(r.verdict, Verdict::Blocked("brain-failure".into()));
    }
    /// The wiring, checked against the real FlyWire brain (not run by default: needs the data).
    /// `hack/fetch-flywire.sh && FLYOPS_BRAIN_DIR=brain cargo test --release -- --ignored --nocapture scenarios`
    #[tokio::test]
    #[ignore]
    async fn scenarios() {
        let dir = std::env::var("FLYOPS_BRAIN_DIR").expect("FLYOPS_BRAIN_DIR");
        let b = crate::connectome::ConnectomeBrainBackend::load(std::path::Path::new(&dir)).unwrap();
        type Case<'a> = (&'a str, &'a [(&'a str, f32)], &'a str); // (scenario, signals, expected response)
        let cases: &[Case] = &[
            ("quiet cluster", &[], "no-op"),
            ("1 of 3 nodes down", &[("node_not_ready", 0.33), ("deployment_degraded", 0.2)], "drain-node"),
            ("1 of 5 nodes down", &[("node_not_ready", 0.2)], "drain-node"),
            ("node down + crashing pods", &[("node_not_ready", 0.33), ("pod_crash_loop", 0.4), ("deployment_degraded", 0.2)], "drain-node"),
            ("one crash-looping pod", &[("pod_crash_loop", 0.2)], "delete-pod"),
            ("crash-looping deployment", &[("pod_crash_loop", 0.6), ("deployment_degraded", 0.2)], "restart-workload"),
            ("degraded deployment", &[("deployment_degraded", 0.2)], "restart-workload"),
            ("memory pressure", &[("node_memory_pressure", 0.33)], "drain-node"),
        ];
        let mut wrong = vec![];
        for (name, s, want) in cases {
            let r = run_cycle(&b, &mut Governor::default(), &sig(s), true).await;
            let pops: Vec<String> = r.output.iter().flat_map(|o| &o.populations)
                .map(|p| format!("{}={:.2}", p.population.trim_start_matches("example-").trim_end_matches("-population"), p.activity)).collect();
            let got = r.candidate.name();
            println!("{:<28} -> {:<17} (want {:<17}) {}", name, got, want, pops.join(" "));
            if got != *want { wrong.push(*name); }
        }
        assert!(wrong.is_empty(), "wrong decisions: {wrong:?}");
    }
    #[test]
    fn wiring_names_are_known() {
        for (s, _, _) in INPUTS { assert!(crate::observer::SIGNALS.contains(s), "unknown signal {s}"); }
        for (p, _, _) in OUTPUTS { assert!(crate::brain::MOCK_POPULATIONS.contains(p), "unknown population {p}"); }
    }
}

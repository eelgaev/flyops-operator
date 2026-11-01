//! FlyOps: an OpenShift operator run by a simulated fruit-fly brain.
mod api;
mod brain;
mod connectome;
mod controller;
mod decoder;
mod encoding;
mod executor;
mod observer;
mod safety;
mod state;
mod web;

use brain::BrainBackend;
use clap::Parser;
use kube::{api::PostParams, runtime::events::Recorder, Api, Client, CustomResourceExt};
use std::{path::{Path, PathBuf}, sync::Arc};

#[derive(Parser)]
#[command(name = "flyops", about = "OpenShift operator run by a simulated fruit-fly brain (experimental)")]
struct Args {
    /// Print the CRDs as YAML and exit.
    #[arg(long)]
    crdgen: bool,
    /// Serve the dashboard with synthetic cluster signals; needs no cluster.
    #[arg(long)]
    demo: bool,
    /// Drive each sensory channel of the brain in DIR and print every readout population, then exit.
    #[arg(long, value_name = "DIR")]
    calibrate: Option<PathBuf>,
    /// Write a small SYNTHETIC brain (not FlyWire) to DIR and exit.
    #[arg(long, value_name = "DIR")]
    gen_synthetic_brain: Option<PathBuf>,
    /// Where the connectome lives (baked into the image).
    #[arg(long, env = "FLYOPS_BRAIN_DIR", default_value = "/opt/flyops/brain")]
    brain_dir: PathBuf,
    /// FlyOps' own namespace: never touched by the fly.
    #[arg(long, env = "POD_NAMESPACE", default_value = "flyops-system")]
    namespace: String,
    #[arg(long, env = "FLYOPS_BIND", default_value = "0.0.0.0:18787")]
    bind: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if let Some(dir) = &args.gen_synthetic_brain {
        connectome::write_synthetic(dir, 3000)?;
        println!("wrote synthetic brain to {}", dir.display());
        return Ok(());
    }
    if args.crdgen {
        for crd in [api::FlyBrain::crd(), api::FlyDecision::crd()] {
            // schemars emits integer formats Kubernetes doesn't know ("uint32"...); the API server warns on every apply.
            let yaml: String = serde_yaml::to_string(&crd)?.lines()
                .filter(|l| !l.trim().starts_with("format: uint"))
                .map(|l| format!("{l}\n")).collect();
            println!("---\n{yaml}");
        }
        return Ok(());
    }
    if let Some(dir) = &args.calibrate {
        return calibrate(dir).await;
    }
    tracing_subscriber::fmt().json().with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse()?)).init();

    let state = Arc::new(state::AppState::new());
    let listener = tokio::net::TcpListener::bind(&args.bind).await?;
    tracing::info!(bind = %args.bind, "serving dashboard");
    let server = axum::serve(listener, web::router(state.clone()));
    if args.demo {
        tokio::spawn(demo(state.clone()));
        server.await?;
        return Ok(());
    }

    let (backend, backend_note) = load_brain(&args.brain_dir).await;
    let client = Client::try_default().await?;
    let brains: Api<api::FlyBrain> = Api::all(client.clone());
    if brains.get_opt(api::BRAIN_NAME).await?.is_none() {
        brains.create(&PostParams::default(), &api::FlyBrain::new(api::BRAIN_NAME, Default::default())).await?;
        tracing::info!("created FlyBrain '{}' (switch OFF)", api::BRAIN_NAME);
    }
    let ctx = controller::Ctx {
        client: client.clone(),
        observer: Arc::new(observer::Observer::start(client.clone(), &args.namespace)),
        state: state.clone(),
        recorder: Recorder::new(client, "flyops".into()),
        own_ns: args.namespace,
        backend,
        backend_note,
    };
    tokio::select! {
        r = server => r?,
        _ = controller::run(ctx) => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    Ok(())
}

/// The real connectome when it is there; otherwise the mock, with a visible note (local runs without data).
async fn load_brain(dir: &Path) -> (Arc<dyn BrainBackend>, Option<String>) {
    let d = dir.to_path_buf();
    let loaded = tokio::task::spawn_blocking(move || connectome::precheck(&d).and_then(|_| connectome::ConnectomeBrainBackend::load(&d))).await;
    match loaded {
        Ok(Ok(b)) => {
            let _ = b.reset(0).await;
            (Arc::new(b), None)
        }
        Ok(Err(e)) => {
            let note = format!("connectome not loaded from {} ({e:#}); using the mock brain", dir.display());
            tracing::warn!("{note}");
            (Arc::new(brain::MockBrainBackend::new()), Some(note))
        }
        Err(e) => (Arc::new(brain::MockBrainBackend::new()), Some(format!("brain loader crashed: {e}"))),
    }
}

async fn calibrate(dir: &Path) -> anyhow::Result<()> {
    let t = std::time::Instant::now();
    let b = connectome::ConnectomeBrainBackend::load(dir)?;
    println!("loaded in {:.2}s: {}", t.elapsed().as_secs_f32(), b.metadata().version);
    let m: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("mapping.json"))?)?;
    let mut channels: Vec<String> = m["channels"].as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
    channels.sort();
    for ch in channels {
        for i in [0.3f32, 1.0] {
            let frame = brain::SensoryFrame { observed_at: chrono::Utc::now(), encoder_version: encoding::ENCODER_VERSION.into(),
                stimuli: vec![brain::SensoryStimulus { channel: ch.clone(), intensity: i, duration_ms: controller::WINDOW_MS }] };
            let o = b.stimulate(frame, controller::WINDOW_MS).await?;
            let row: Vec<String> = o.populations.iter().map(|p| format!("{}={:.2}", p.population.replace("example-", "").replace("-population", ""), p.activity)).collect();
            println!("{ch:>15} @{i:.1}: {}", row.join("  "));
        }
    }
    Ok(())
}

/// Synthetic incident script so the dashboard has something to show without a cluster. The switch is OFF.
async fn demo(state: Arc<state::AppState>) {
    let backend = brain::MockBrainBackend::new();
    let mut gov = safety::Governor::default();
    let mut hyst = encoding::Hysteresis::default();
    let mut hist = std::collections::VecDeque::new();
    let mut last = String::new();
    let mut t = 0u64;
    loop {
        t += 1;
        let x = t as f32 * 0.15;
        let burst = if (t % 60) > 35 && (t % 60) < 50 { 1.0 } else { 0.0 }; // a "node outage" every ~60 ticks
        let sig: encoding::Signals = [
            ("node_not_ready", 0.9 * burst),
            ("node_memory_pressure", 0.25 * (x.sin() * 0.5 + 0.5) + 0.3 * burst),
            ("pod_crash_loop", 0.35 * ((x * 0.7).sin() * 0.5 + 0.5)),
            ("pod_pending_duration", 0.2 * ((x * 0.4 + 1.0).sin() * 0.5 + 0.5)),
            ("deployment_degraded", 0.3 * ((x * 0.5 + 2.0).sin() * 0.5 + 0.5) + 0.4 * burst),
            ("job_failed", if t % 23 < 4 { 0.6 } else { 0.0 }),
        ].into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        let sig = hyst.apply(&sig);
        let r = controller::run_cycle(&backend, &mut gov, &sig, false).await;
        let resp = r.candidate.name().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        if resp != last {
            hist.push_front(state::DecisionView { time: now.clone(), response: resp.clone(), detail: String::new(), safety: r.verdict.label(), outcome: String::new() });
            hist.truncate(30);
        }
        last = resp.clone();
        state.publish(state::BrainView {
            name: "demo".into(), switch: "OFF".into(), backend: "mock".into(), model: "linear-mock".into(), demo: true,
            updated_at: now, ticks: t, signals: sig, stimuli: r.frame.stimuli.iter().map(|s| (s.channel.clone(), s.intensity)).collect(),
            populations: r.output.map(|o| o.populations.into_iter().map(|p| (p.population, p.activity)).collect()).unwrap_or_default(),
            response: resp, safety: r.verdict.label(), error: None,
            counts: observer::ObservedCounts { nodes: 6, pods: 142, deployments: 38, jobs: 5 }, history: hist.clone(),
        });
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

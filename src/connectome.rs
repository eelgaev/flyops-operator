//! Connectome-constrained leaky integrate-and-fire (LIF) backend.
//!
//! Model and parameters follow the public reference implementation
//! <https://github.com/philshiu/Drosophila_brain_model> (`model.py`, MIT license; Shiu et al. 2023):
//! v0 = v_rst = -52 mV, v_th = -45 mV, t_mbr = 20 ms, tau_syn = 5 ms, t_rfc = 2.2 ms, t_dly = 1.8 ms,
//! w_syn = 0.275 mV per synapse, Poisson input 150 Hz with weight w_syn * 250 applied to v.
//! This is a re-implementation (explicit Euler, dt = 0.1 ms), NOT the upstream simulator: validate against upstream
//! examples before drawing any conclusion from its activity (plan Phase 4).
//!
//! What is *measured* (the wiring, from FlyWire) vs *assumed* (everything else):
//! - measured: which neuron synapses onto which, synapse counts and the sign column of the dataset;
//! - assumed: the neuron/synapse dynamics above, and the project-defined mapping in `mapping.json`
//!   from FlyOps sensory channels to stimulated neurons and from neuron groups to readout populations.
use anyhow::{bail, Context, Result};
use crate::brain::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, io::Read, path::Path, sync::Arc};

pub const DT_MS: f32 = 0.1;
const V0: f32 = -52.0;
const V_RST: f32 = -52.0;
const V_TH: f32 = -45.0;
const T_MBR: f32 = 20.0;
const TAU_SYN: f32 = 5.0;
const T_RFC: f32 = 2.2;
const T_DLY: f32 = 1.8;
pub const W_SYN: f32 = 0.275;
const F_POI: f32 = 250.0;

/// `mapping.json`: engineered, project-defined interface between FlyOps and the connectome.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mapping {
    #[serde(default = "d_neurons")]
    pub neurons_file: String,
    #[serde(default = "d_conns")]
    pub connections_file: String,
    /// Free-text provenance, e.g. dataset release and who chose the mapping.
    #[serde(default)]
    pub description: String,
    /// Poisson rate at intensity 1.0.
    #[serde(default = "d_rate")]
    pub max_input_rate_hz: f32,
    /// A population whose mean firing rate reaches this reads out as activity 1.0.
    #[serde(default = "d_ref")]
    pub activity_ref_hz: f32,
    /// Per-population override of `activity_ref_hz` (populations differ a lot in how fast they fire).
    #[serde(default)]
    pub population_ref_hz: HashMap<String, f32>,
    /// channel name -> FlyWire root ids that receive Poisson input.
    pub channels: HashMap<String, Vec<u64>>,
    /// population name -> FlyWire root ids that are read out.
    pub populations: HashMap<String, Vec<u64>>,
}
fn d_neurons() -> String { "Completeness_783.csv".into() }
fn d_conns() -> String { "Connectivity_783.csv".into() }
fn d_rate() -> f32 { 150.0 }
fn d_ref() -> f32 { 20.0 }

/// Compressed sparse row graph; `w` is signed and already multiplied by W_SYN (mV).
pub struct Connectome {
    pub n: usize,
    ptr: Vec<u32>,
    post: Vec<u32>,
    w: Vec<f32>,
    pub provenance: String,
}

impl Connectome {
    /// `edges`: (pre_index, post_index, signed synapse count).
    /// Edges are sorted in place (by pre, then post) so the CSR order is deterministic regardless of input order.
    #[cfg(test)]
    pub fn from_edges(n: usize, mut edges: Vec<(u32, u32, f32)>, provenance: String) -> Result<Self> {
        if edges.len() > u32::MAX as usize { bail!("{} edges exceed the u32 CSR limit", edges.len()); }
        let mut ptr = vec![0u32; n + 1];
        for &(a, b, _) in &edges {
            if a as usize >= n || b as usize >= n { bail!("edge ({a},{b}) outside neuron range 0..{n}"); }
            ptr[a as usize + 1] += 1;
        }
        for i in 0..n { ptr[i + 1] += ptr[i]; }
        edges.sort_unstable_by_key(|&(a, b, _)| (a, b));
        let (post, w): (Vec<u32>, Vec<f32>) = edges.into_iter().map(|(_, b, c)| (b, c * W_SYN)).unzip();
        Ok(Self::from_csr(n, ptr, post, w, provenance))
    }
    /// `w` already scaled by W_SYN. Rows keep file order, so a given file always yields the same graph.
    fn from_csr(n: usize, ptr: Vec<u32>, post: Vec<u32>, w: Vec<f32>, provenance: String) -> Self {
        Self { n, ptr, post, w, provenance }
    }
}

pub struct Dataset {
    pub connectome: Connectome,
    pub channels: HashMap<String, Vec<u32>>,
    /// (name, neuron indices, reference rate in Hz that reads out as activity 1.0)
    pub populations: Vec<(String, Vec<u32>, f32)>,
    pub max_input_rate_hz: f32,
}

fn open(path: &Path) -> Result<Box<dyn Read>> {
    let f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    Ok(if path.extension().is_some_and(|e| e == "gz") { Box::new(flate2::read::GzDecoder::new(f)) } else { Box::new(f) })
}

fn sha256_prefix(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut h)?;
    Ok(h.finalize().iter().take(6).map(|b| format!("{b:02x}")).collect())
}

/// Cheap validation used by the reconciler (does not load the graph).
pub fn precheck(dir: &Path) -> Result<()> {
    let m: Mapping = serde_json::from_reader(std::fs::File::open(dir.join("mapping.json")).context("mapping.json")?)?;
    for f in [&m.neurons_file, &m.connections_file] {
        if !dir.join(f).is_file() { bail!("missing data file {}", dir.join(f).display()); }
    }
    if m.channels.is_empty() || m.populations.is_empty() { bail!("mapping.json needs at least one channel and one population"); }
    Ok(())
}

/// Stream (pre, post, signed synapse count) for every non-zero row of the connections file.
/// `.parquet` (as published upstream) or `.csv` / `.csv.gz`.
fn for_each_edge(conns_path: &Path, mut f: impl FnMut(u32, u32, f32) -> Result<()>) -> Result<()> {
    const COLS: [&str; 3] = ["Presynaptic_Index", "Postsynaptic_Index", "Excitatory x Connectivity"];
    if conns_path.extension().is_some_and(|e| e == "parquet") {
        use parquet::{column::reader::get_typed_column_reader, data_type::Int64Type, file::reader::{FileReader, SerializedFileReader}};
        let r = SerializedFileReader::new(std::fs::File::open(conns_path).with_context(|| format!("opening {}", conns_path.display()))?)?;
        let schema = r.metadata().file_metadata().schema_descr();
        let idx: Vec<usize> = COLS.iter().map(|name| (0..schema.num_columns()).find(|&i| schema.column(i).name() == *name)
            .with_context(|| format!("column '{name}' missing in {}", conns_path.display()))).collect::<Result<_>>()?;
        for g in 0..r.num_row_groups() {
            let rg = r.get_row_group(g)?;
            let rows = rg.metadata().num_rows() as usize;
            let mut cols: Vec<Vec<i64>> = Vec::with_capacity(3);
            for &ci in &idx {
                let mut rd = get_typed_column_reader::<Int64Type>(rg.get_column_reader(ci)?);
                let (mut vals, mut defs) = (Vec::with_capacity(rows), Vec::with_capacity(rows));
                while vals.len() < rows {
                    let (recs, _, _) = rd.read_records(rows - vals.len(), Some(&mut defs), None, &mut vals)?;
                    if recs == 0 { break; }
                }
                if vals.len() != rows { bail!("{}: row group {g} column {} has nulls or is short ({} of {rows})", conns_path.display(), COLS[cols.len()], vals.len()); }
                cols.push(vals);
            }
            for ((&a, &b), &w) in cols[0].iter().zip(&cols[1]).zip(&cols[2]) {
                if w != 0 { f(u32::try_from(a)?, u32::try_from(b)?, w as f32)?; }
            }
        }
    } else {
        let mut rdr = csv::Reader::from_reader(open(conns_path)?);
        let hdr = rdr.headers()?.clone();
        let col = |name: &str| hdr.iter().position(|h| h == name).with_context(|| format!("column '{name}' missing in {}", conns_path.display()));
        let (ci, cj, cw) = (col(COLS[0])?, col(COLS[1])?, col(COLS[2])?);
        for rec in rdr.records() {
            let r = rec?;
            let w: f32 = r[cw].trim().parse()?;
            if w != 0.0 { f(r[ci].trim().parse::<u32>()?, r[cj].trim().parse::<u32>()?, w)?; }
        }
    }
    Ok(())
}

/// Load the neuron list and the graph. Neurons file: first column = FlyWire root id, one row per neuron,
/// row position = neuron index. Returns the graph and the root-id -> index map.
pub fn load_graph(neurons_path: &Path, conns_path: &Path) -> Result<(Connectome, HashMap<u64, u32>)> {
    let mut id_to_idx: HashMap<u64, u32> = HashMap::new();
    let mut rdr = csv::ReaderBuilder::new().flexible(true).from_reader(open(neurons_path)?);
    for (i, rec) in rdr.records().enumerate() {
        let rec = rec?;
        let id: u64 = rec.get(0).context("empty neuron row")?.trim().parse().with_context(|| format!("neuron row {i}: bad root id"))?;
        id_to_idx.insert(id, i as u32);
    }
    let n = id_to_idx.len();
    if n == 0 { bail!("no neurons in {}", neurons_path.display()); }
    // Two passes (count, then fill) so the 15M-edge FlyWire graph never exists as a temporary edge list.
    let mut ptr = vec![0u32; n + 1];
    for_each_edge(conns_path, |a, b, _| {
        if a as usize >= n || b as usize >= n { bail!("edge ({a},{b}) outside neuron range 0..{n}"); }
        ptr[a as usize + 1] = ptr[a as usize + 1].checked_add(1).context("too many edges for u32 CSR")?;
        Ok(())
    })?;
    for i in 0..n { ptr[i + 1] = ptr[i + 1].checked_add(ptr[i]).context("too many edges for u32 CSR")?; }
    let m = ptr[n] as usize;
    let (mut post, mut w) = (vec![0u32; m], vec![0f32; m]);
    let mut fill: Vec<u32> = ptr[..n].to_vec();
    for_each_edge(conns_path, |a, b, c| {
        let k = &mut fill[a as usize];
        if *k >= ptr[a as usize + 1] { bail!("{} changed between passes", conns_path.display()); }
        post[*k as usize] = b;
        w[*k as usize] = c * W_SYN;
        *k += 1;
        Ok(())
    })?;
    let provenance = format!("{n}n/{m}e/sha256:{}", sha256_prefix(conns_path)?);
    Ok((Connectome::from_csr(n, ptr, post, w, provenance), id_to_idx))
}

/// Load a dataset directory laid out like the upstream repo: `mapping.json` plus the neuron and connection files.
pub fn load(dir: &Path) -> Result<Dataset> {
    let m: Mapping = serde_json::from_reader(std::fs::File::open(dir.join("mapping.json")).context("mapping.json")?)?;
    let (connectome, id_to_idx) = load_graph(&dir.join(&m.neurons_file), &dir.join(&m.connections_file))?;

    let resolve = |what: &str, name: &str, ids: &[u64]| -> Result<Vec<u32>> {
        let v: Vec<u32> = ids.iter().filter_map(|id| id_to_idx.get(id).copied()).collect();
        if v.len() != ids.len() { bail!("{what} '{name}': {} of {} root ids not found in the neuron file", ids.len() - v.len(), ids.len()); }
        if v.is_empty() { bail!("{what} '{name}' is empty"); }
        Ok(v)
    };
    let mut channels = HashMap::new();
    for (k, ids) in &m.channels { channels.insert(k.clone(), resolve("channel", k, ids)?); }
    let mut populations = Vec::new();
    let mut names: Vec<_> = m.populations.keys().cloned().collect();
    names.sort();
    for k in names {
        let r = m.population_ref_hz.get(&k).copied().unwrap_or(m.activity_ref_hz);
        if !(r.is_finite() && r > 0.0) { bail!("population '{k}': reference rate must be > 0 Hz"); }
        populations.push((k.clone(), resolve("population", &k, &m.populations[&k])?, r));
    }
    if let Some(k) = m.population_ref_hz.keys().find(|k| !m.populations.contains_key(*k)) { bail!("population_ref_hz names unknown population '{k}'"); }
    Ok(Dataset { connectome, channels, populations, max_input_rate_hz: m.max_input_rate_hz })
}

// -------------------------------------------------------------------------- simulation

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 { self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0 }
    fn unit(&mut self) -> f32 { (self.next() >> 40) as f32 / (1u64 << 24) as f32 }
}

/// Residual distance from rest (mV) below which a neuron is snapped back to exact rest and dropped from the
/// active set. 7 mV separate rest from threshold, so this changes no spike; it keeps quiet neurons from being
/// integrated forever while their synaptic current decays toward (but never reaches) zero.
const REST_EPS: f32 = 1e-4;

/// Simulate `duration_ms` from rest. `drive`: (neuron index, Poisson rate Hz). Returns the spike count per neuron.
/// Pure function of its inputs (seeded), so the same frame and seed give the same output.
///
/// Event-driven over an active-set bitset: only neurons that received input, are refractory, or have not yet decayed
/// back to rest are integrated. Idle 64-neuron blocks cost one word test per step, active neurons are visited in
/// index order (cache-friendly, and the same order as a full scan), so cost scales with activity, not brain size.
pub fn simulate(c: &Connectome, drive: &[(u32, f32)], duration_ms: u32, seed: u64) -> Vec<u32> {
    let steps = (duration_ms as f32 / DT_MS) as usize;
    let delay = (T_DLY / DT_MS).round() as usize; // 18 steps
    let rfc_steps = (T_RFC / DT_MS).round() as i32;
    let n = c.n;
    let (mut v, mut g) = (vec![V0; n], vec![0f32; n]);
    let mut rfc = vec![0i32; n];
    let mut stim_no_rfc = vec![false; n];
    for &(i, _) in drive { stim_no_rfc[i as usize] = true; }
    let mut counts = vec![0u32; n];
    let mut ring: Vec<Vec<u32>> = vec![Vec::new(); delay + 1];
    let mut active = vec![0u64; n.div_ceil(64)];
    let mut rng = Rng(seed ^ 0x9E37_79B9_7F4A_7C15 | 1);
    let (dec_v, dec_g) = (DT_MS / T_MBR, (-DT_MS / TAU_SYN).exp());
    let poi_w = W_SYN * F_POI;

    for t in 0..steps {
        // 1) deliver spikes that were emitted `delay` steps ago
        let slot = t % (delay + 1);
        let arriving = std::mem::take(&mut ring[slot]);
        for &s in &arriving {
            for k in c.ptr[s as usize] as usize..c.ptr[s as usize + 1] as usize {
                let p = c.post[k];
                g[p as usize] += c.w[k];
                active[p as usize >> 6] |= 1 << (p & 63);
            }
        }
        // 2) Poisson input jumps v directly (as upstream: target v)
        for &(i, rate) in drive {
            if rng.unit() < rate * DT_MS * 1e-3 { v[i as usize] += poi_w; active[i as usize >> 6] |= 1 << (i & 63); }
        }
        // 3) integrate the active set (not while refractory), detect spikes, retire neurons back at rest
        let mut fired = arriving; // reuse allocation
        fired.clear();
        for (wi, word) in active.iter_mut().enumerate() {
            let mut m = *word;
            while m != 0 {
                let b = m.trailing_zeros();
                m &= m - 1;
                let i = wi * 64 + b as usize;
                if rfc[i] > 0 { rfc[i] -= 1; continue; }
                v[i] += dec_v * (V0 - v[i] + g[i]);
                g[i] *= dec_g;
                if v[i] > V_TH {
                    v[i] = V_RST;
                    g[i] = 0.0;
                    rfc[i] = if stim_no_rfc[i] { 0 } else { rfc_steps };
                    counts[i] += 1;
                    fired.push(i as u32);
                }
                if rfc[i] == 0 && g[i].abs() < REST_EPS && (v[i] - V0).abs() < REST_EPS {
                    v[i] = V0;
                    g[i] = 0.0;
                    *word &= !(1u64 << b);
                }
            }
        }
        ring[slot] = fired; // delivered `delay + 1` steps later: slot reused after delay+1 ticks
    }
    counts
}

pub struct ConnectomeBrainBackend {
    ds: Arc<Dataset>,
    seed: std::sync::atomic::AtomicU64,
}

impl ConnectomeBrainBackend {
    pub fn new(ds: Dataset) -> Self { Self { ds: Arc::new(ds), seed: Default::default() } }
    /// Blocking load (reads and indexes the whole graph). Call from `spawn_blocking`.
    pub fn load(dir: &Path) -> Result<Self> { Ok(Self::new(load(dir)?)) }
}

#[async_trait::async_trait]
impl BrainBackend for ConnectomeBrainBackend {
    fn metadata(&self) -> BrainMetadata {
        BrainMetadata { backend: "connectome".into(), model: "lif-shiu2023-reimpl".into(), version: self.ds.connectome.provenance.clone() }
    }
    async fn reset(&self, seed: u64) -> Result<()> {
        self.seed.store(seed, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
    async fn stimulate(&self, input: SensoryFrame, duration_ms: u32) -> Result<NeuralOutput> {
        let ds = self.ds.clone();
        let seed = self.seed.load(std::sync::atomic::Ordering::Relaxed);
        tokio::task::spawn_blocking(move || {
            // combine intensities per channel, then per neuron (unknown channels are ignored, not errors)
            let mut rate: HashMap<u32, f32> = HashMap::new();
            let mut h = seed;
            for s in &input.stimuli {
                if let Some(ids) = ds.channels.get(&s.channel) {
                    for &i in ids { *rate.entry(i).or_insert(0.0) += s.intensity.clamp(0.0, 1.0) * ds.max_input_rate_hz; }
                }
                h = h.wrapping_mul(1099511628211) ^ s.intensity.to_bits() as u64 ^ s.channel.len() as u64;
            }
            let mut drive: Vec<(u32, f32)> = rate.into_iter().collect();
            drive.sort_by_key(|d| d.0); // deterministic order
            let counts = simulate(&ds.connectome, &drive, duration_ms, h);
            let secs = duration_ms as f32 / 1000.0;
            let populations = ds.populations.iter().map(|(name, ids, ref_hz)| {
                let spikes: u64 = ids.iter().map(|&i| counts[i as usize] as u64).sum();
                let hz = spikes as f32 / ids.len() as f32 / secs;
                PopulationActivity { population: name.clone(), activity: (hz / ref_hz).clamp(0.0, 1.0) }
            }).collect();
            Ok(NeuralOutput { simulated_ms: duration_ms, populations })
        }).await?
    }
}

// -------------------------------------------------------------------------- synthetic data

/// Write a small SYNTHETIC dataset in the upstream CSV layout so the whole pipeline can run without FlyWire.
/// Three layers (sensory -> hidden -> readout). Not a fly; for testing and demos only.
pub fn write_synthetic(dir: &Path, n: usize) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let channels = ["looming", "strong-looming", "abrupt", "odor-left", "odor-right", "temperature", "touch"];
    let pops = MOCK_POPULATIONS;
    let per = (n / 20).max(4);
    let sens_end = per * channels.len();
    let out_start = n - per * pops.len();
    anyhow::ensure!(sens_end + 50 < out_start, "n too small");
    let base: u64 = 720_575_940_000_000_000;
    let mut nf = String::from("root_id,Completed\n");
    for i in 0..n { nf += &format!("{},True\n", base + i as u64); }
    std::fs::write(dir.join("Completeness_783.csv"), nf)?;
    let mut rng = Rng(42);
    let mut cf = String::from("Presynaptic_Index,Postsynaptic_Index,Excitatory x Connectivity\n");
    let hidden = (sens_end, out_start);
    let pick = |rng: &mut Rng, lo: usize, hi: usize| lo + (rng.next() % (hi - lo) as u64) as usize;
    // each hidden neuron listens to ~12 sensory neurons, each output neuron to ~30 hidden ones; a few inhibitory edges
    for h in hidden.0..hidden.1 {
        for _ in 0..40 { cf += &format!("{},{h},{}\n", pick(&mut rng, 0, sens_end), 6); }
    }
    for o in out_start..n {
        for _ in 0..40 { cf += &format!("{},{o},{}\n", pick(&mut rng, hidden.0, hidden.1), if rng.next().is_multiple_of(10) { -6 } else { 8 }); }
    }
    std::fs::write(dir.join("Connectivity_783.csv"), cf)?;
    let ids = |lo: usize, hi: usize| (lo..hi).map(|i| base + i as u64).collect::<Vec<_>>();
    let m = Mapping {
        neurons_file: d_neurons(), connections_file: d_conns(), max_input_rate_hz: 150.0, activity_ref_hz: 20.0, population_ref_hz: HashMap::new(),
        description: "SYNTHETIC toy network for pipeline testing. Not FlyWire data.".into(),
        channels: channels.iter().enumerate().map(|(k, c)| (c.to_string(), ids(k * per, (k + 1) * per))).collect(),
        populations: pops.iter().enumerate().map(|(k, p)| (p.to_string(), ids(out_start + k * per, out_start + (k + 1) * per))).collect(),
    };
    std::fs::write(dir.join("mapping.json"), serde_json::to_vec_pretty(&m)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("flyops-conn-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }
    fn frame(ch: &str, i: f32) -> SensoryFrame {
        SensoryFrame { observed_at: chrono_zero(), encoder_version: "t".into(), stimuli: vec![SensoryStimulus { channel: ch.into(), intensity: i, duration_ms: 250 }] }
    }
    fn chrono_zero() -> chrono::DateTime<chrono::Utc> { chrono::DateTime::from_timestamp(0, 0).unwrap() }

    #[test]
    fn delivery_and_sign() {
        // 0 --(+200 syn)--> 1 ; 0 --(-200)--> 2.  Drive neuron 0 hard.
        let c = Connectome::from_edges(3, vec![(0, 1, 200.0), (0, 2, -200.0)], "t".into()).unwrap();
        let counts = simulate(&c, &[(0, 500.0)], 200, 1);
        assert!(counts[0] > 0 && counts[1] > 0, "excitatory target should fire: {counts:?}");
        assert_eq!(counts[2], 0, "inhibitory target must not fire");
    }
    #[test]
    fn no_input_no_activity() {
        let c = Connectome::from_edges(3, vec![(0, 1, 200.0)], "t".into()).unwrap();
        assert_eq!(simulate(&c, &[], 100, 1), vec![0, 0, 0]);
    }
    #[test]
    fn deterministic() {
        let c = Connectome::from_edges(3, vec![(0, 1, 5.0), (1, 2, 5.0)], "t".into()).unwrap();
        assert_eq!(simulate(&c, &[(0, 150.0)], 250, 9), simulate(&c, &[(0, 150.0)], 250, 9));
    }
    #[test]
    fn bad_edge_rejected() { assert!(Connectome::from_edges(2, vec![(0, 5, 1.0)], "t".into()).is_err()); }

    #[tokio::test]
    async fn synthetic_dataset_end_to_end() {
        let d = tmp("e2e");
        write_synthetic(&d, 3000).unwrap();
        precheck(&d).unwrap();
        let b = ConnectomeBrainBackend::load(&d).unwrap();
        assert!(b.metadata().version.starts_with("3000n/"));
        b.reset(1).await.unwrap();
        let act = |o: &NeuralOutput, p: &str| o.populations.iter().find(|x| x.population == p).unwrap().activity;
        let quiet = b.stimulate(frame("looming", 0.0), 250).await.unwrap();
        let weak = b.stimulate(frame("looming", 0.2), 250).await.unwrap();
        let strong = b.stimulate(frame("looming", 1.0), 250).await.unwrap();
        let again = b.stimulate(frame("looming", 1.0), 250).await.unwrap();
        assert_eq!(strong, again, "same frame + seed must replay identically");
        assert_eq!(act(&quiet, "example-descending-population"), 0.0);
        println!("weak={:?}\nstrong={:?}", weak, strong);
        let total = |o: &NeuralOutput| o.populations.iter().map(|p| p.activity).sum::<f32>();
        assert!(total(&strong) >= total(&weak), "stronger input should not reduce output");
        assert!(total(&weak) < 5.0, "synthetic net should not saturate on weak input");
        assert!(total(&strong) > 0.0, "strong input should reach the readout layer");
        let _ = std::fs::remove_dir_all(&d);
    }
    #[tokio::test]
    async fn per_population_reference_rates() {
        let d = tmp("ref");
        write_synthetic(&d, 3000).unwrap();
        let mut m: Mapping = serde_json::from_slice(&std::fs::read(d.join("mapping.json")).unwrap()).unwrap();
        let p = "example-descending-population";
        let activity = |d: std::path::PathBuf| async move {
            let o = ConnectomeBrainBackend::load(&d).unwrap().stimulate(frame("looming", 1.0), 250).await.unwrap();
            o.populations.iter().find(|x| x.population == p).unwrap().activity
        };
        let a0 = activity(d.clone()).await;
        assert!(a0 > 0.0 && a0 < 1.0, "need an unsaturated baseline, got {a0}");
        m.population_ref_hz.insert(p.into(), m.activity_ref_hz * 4.0);
        std::fs::write(d.join("mapping.json"), serde_json::to_vec(&m).unwrap()).unwrap();
        let a1 = activity(d.clone()).await;
        assert!((a1 - a0 / 4.0).abs() < 1e-6, "4x reference rate should quarter the activity: {a0} -> {a1}");
        m.population_ref_hz.insert("no-such-population".into(), 1.0);
        std::fs::write(d.join("mapping.json"), serde_json::to_vec(&m).unwrap()).unwrap();
        assert!(load(&d).is_err(), "unknown population in population_ref_hz must be rejected");
        let _ = std::fs::remove_dir_all(&d);
    }
    #[test]
    fn missing_ids_are_reported() {
        let d = tmp("missing");
        write_synthetic(&d, 1000).unwrap();
        let mut m: Mapping = serde_json::from_slice(&std::fs::read(d.join("mapping.json")).unwrap()).unwrap();
        m.channels.get_mut("looming").unwrap().push(1);
        std::fs::write(d.join("mapping.json"), serde_json::to_vec(&m).unwrap()).unwrap();
        let e = load(&d).err().unwrap().to_string();
        assert!(e.contains("not found"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }
}

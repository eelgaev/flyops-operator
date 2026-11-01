//! Cluster signals -> SensoryFrame. Channel names are engineered labels, not fly anatomy.
use crate::brain::{SensoryFrame, SensoryStimulus};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Bump on any scaling or mapping change; recorded in every decision.
pub const ENCODER_VERSION: &str = "v1";

/// Normalised signals, each in [0,1]. Missing = unknown (never treated as zero).
pub type Signals = BTreeMap<String, f32>;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct InputMapping {
    pub signal: String,
    pub channel: String,
    pub gain: f32,
}

pub fn clamp01(v: f32) -> f32 {
    if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) }
}

/// Saturating count -> [0,1]: `count / full_scale`.
pub fn normalize_count(count: usize, full_scale: usize) -> f32 {
    clamp01(count as f32 / full_scale.max(1) as f32)
}

pub fn encode(
    signals: &Signals,
    mappings: &[InputMapping],
    observed_at: chrono::DateTime<chrono::Utc>,
    duration_ms: u32,
    max_stimuli: usize,
) -> SensoryFrame {
    let stimuli = mappings
        .iter()
        .filter_map(|m| {
            let v = *signals.get(&m.signal)?; // unknown signal -> no stimulus
            let intensity = clamp01(clamp01(v) * m.gain);
            (intensity > 0.0).then(|| SensoryStimulus { channel: m.channel.clone(), intensity, duration_ms })
        })
        .take(max_stimuli)
        .collect();
    SensoryFrame { observed_at, encoder_version: ENCODER_VERSION.into(), stimuli }
}

/// Schmitt-trigger hysteresis per signal to stop flapping conditions.
#[derive(Default)]
pub struct Hysteresis {
    on: BTreeMap<String, bool>,
}

impl Hysteresis {
    const HI: f32 = 0.15;
    const LO: f32 = 0.05;
    /// Signals below LO are forced to 0 once off; a signal turns on only above HI; once on it stays until below LO.
    pub fn apply(&mut self, signals: &Signals) -> Signals {
        signals
            .iter()
            .map(|(k, &v)| {
                let st = self.on.entry(k.clone()).or_insert(false);
                *st = if *st { v >= Self::LO } else { v >= Self::HI };
                (k.clone(), if *st { v } else { 0.0 })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn t0() -> chrono::DateTime<chrono::Utc> { chrono::DateTime::from_timestamp(0, 0).unwrap() }
    fn m(s: &str, c: &str, g: f32) -> InputMapping { InputMapping { signal: s.into(), channel: c.into(), gain: g } }

    #[test]
    fn clamps_and_skips_unknown() {
        let sig: Signals = [("a".to_string(), 2.0), ("b".to_string(), 0.5)].into();
        let f = encode(&sig, &[m("a", "x", 0.8), m("missing", "y", 1.0), m("b", "z", 4.0)], t0(), 10, 10);
        assert_eq!(f.stimuli.len(), 2);
        assert_eq!(f.stimuli[0].intensity, 0.8);
        assert_eq!(f.stimuli[1].intensity, 1.0);
    }
    #[test]
    fn caps_stimuli() {
        let sig: Signals = [("a".to_string(), 1.0)].into();
        assert_eq!(encode(&sig, &[m("a", "x", 1.0), m("a", "y", 1.0)], t0(), 10, 1).stimuli.len(), 1);
    }
    #[test]
    fn nan_is_zero() { assert_eq!(clamp01(f32::NAN), 0.0); }
    #[test]
    fn hysteresis_holds() {
        let mut h = Hysteresis::default();
        let s = |v| -> Signals { [("a".to_string(), v)].into() };
        assert_eq!(h.apply(&s(0.1))["a"], 0.0); // below HI
        assert_eq!(h.apply(&s(0.2))["a"], 0.2); // on
        assert_eq!(h.apply(&s(0.1))["a"], 0.1); // stays on above LO
        assert_eq!(h.apply(&s(0.01))["a"], 0.0); // off
    }
}

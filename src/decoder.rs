//! Neural output -> candidate response. Only a fixed set of response names is decodable.
use crate::brain::{CandidateResponse, NeuralOutput};

#[derive(Clone, Debug, PartialEq)]
pub struct OutputMapping {
    pub population: String,
    pub response: String,
    pub threshold: f32,
}

#[derive(Debug, PartialEq)]
pub enum DecodeError {
    InvalidOutput(String),
}

/// Validates the output (finite, within [0,1], bounded size) then picks the FIRST mapping whose threshold is met:
/// `mappings` is in priority order (like a fly, escape preempts everything else). Unknown names decode to NoOp.
pub fn decode(out: &NeuralOutput, mappings: &[OutputMapping]) -> Result<CandidateResponse, DecodeError> {
    if out.populations.len() > 10_000 {
        return Err(DecodeError::InvalidOutput("too many populations".into()));
    }
    for p in &out.populations {
        if !p.activity.is_finite() || !(0.0..=1.0).contains(&p.activity) {
            return Err(DecodeError::InvalidOutput(format!("activity out of range for {}", p.population)));
        }
    }
    let first = mappings.iter().find_map(|m| {
        let a = out.populations.iter().find(|p| p.population == m.population)?.activity;
        (a >= m.threshold).then_some((a, m))
    });
    Ok(match first {
        None => CandidateResponse::NoOp,
        Some((a, m)) => {
            let why = format!("{} activity {:.2} >= {:.2}", m.population, a, m.threshold);
            match m.response.as_str() {
                "raise-alert" => CandidateResponse::RaiseAlert { reason: why },
                "delete-pod" => CandidateResponse::DeletePod,
                "restart-workload" => CandidateResponse::RestartWorkload,
                "drain-node" => CandidateResponse::DrainNode,
                _ => CandidateResponse::NoOp,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brain::PopulationActivity;
    fn out(a: f32) -> NeuralOutput { NeuralOutput { simulated_ms: 1, populations: vec![PopulationActivity { population: "p".into(), activity: a }] } }
    fn map(t: f32, r: &str) -> Vec<OutputMapping> { vec![OutputMapping { population: "p".into(), response: r.into(), threshold: t }] }
    #[test]
    fn below_threshold_is_noop() { assert_eq!(decode(&out(0.3), &map(0.7, "raise-alert")), Ok(CandidateResponse::NoOp)); }
    #[test]
    fn above_threshold_alerts() { assert!(matches!(decode(&out(0.9), &map(0.7, "raise-alert")), Ok(CandidateResponse::RaiseAlert { .. }))); }
    #[test]
    fn unknown_response_is_noop() { assert_eq!(decode(&out(0.9), &map(0.7, "rm-rf")), Ok(CandidateResponse::NoOp)); }
    #[test]
    fn actions_decode() {
        assert_eq!(decode(&out(0.9), &map(0.7, "delete-pod")), Ok(CandidateResponse::DeletePod));
        assert_eq!(decode(&out(0.9), &map(0.7, "restart-workload")), Ok(CandidateResponse::RestartWorkload));
        assert_eq!(decode(&out(0.9), &map(0.7, "drain-node")), Ok(CandidateResponse::DrainNode));
    }
    #[test]
    fn earlier_mapping_wins_even_if_weaker() {
        let o = NeuralOutput { simulated_ms: 1, populations: vec![
            PopulationActivity { population: "a".into(), activity: 0.3 }, PopulationActivity { population: "b".into(), activity: 0.9 }] };
        let m = vec![OutputMapping { population: "a".into(), response: "drain-node".into(), threshold: 0.2 },
                     OutputMapping { population: "b".into(), response: "delete-pod".into(), threshold: 0.5 }];
        assert_eq!(decode(&o, &m), Ok(CandidateResponse::DrainNode));
    }
    #[test]
    fn invalid_output_rejected() {
        assert!(decode(&out(f32::NAN), &map(0.7, "raise-alert")).is_err());
        assert!(decode(&out(1.5), &map(0.7, "raise-alert")).is_err());
    }
}

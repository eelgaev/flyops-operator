//! Safety governor: deterministic, independent of the brain, fails closed.
//!
//! - Switch OFF: no action, ever.
//! - Brain failed, timed out or produced invalid output: no action.
//! - At most one action per ACTION_GAP; alerts (Events only) at most one per ALERT_GAP.
use crate::brain::CandidateResponse;
use std::time::{Duration, Instant};

pub const ACTION_GAP: Duration = Duration::from_secs(60);
pub const ALERT_GAP: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allowed,
    Blocked(String),
}

impl Verdict {
    pub fn label(&self) -> String {
        match self {
            Verdict::Allowed => "allowed".into(),
            Verdict::Blocked(r) => format!("blocked:{r}"),
        }
    }
}

#[derive(Default)]
pub struct Governor {
    last_action: Option<Instant>,
    last_alert: Option<Instant>,
}

impl Governor {
    pub fn evaluate(&mut self, c: &CandidateResponse, switch_on: bool, brain_failed: bool, now: Instant) -> Verdict {
        let since = |t: Option<Instant>| t.map(|t| now.duration_since(t));
        if brain_failed {
            return Verdict::Blocked("brain-failure".into());
        }
        match c {
            CandidateResponse::NoOp => Verdict::Blocked("no-op".into()),
            CandidateResponse::RaiseAlert { .. } => {
                if since(self.last_alert).is_some_and(|d| d < ALERT_GAP) {
                    return Verdict::Blocked("alert-rate-limit".into());
                }
                self.last_alert = Some(now);
                Verdict::Allowed
            }
            _ => {
                if !switch_on {
                    return Verdict::Blocked("switch-off".into());
                }
                if since(self.last_action).is_some_and(|d| d < ACTION_GAP) {
                    return Verdict::Blocked("cooldown".into());
                }
                self.last_action = Some(now);
                Verdict::Allowed
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use CandidateResponse::*;

    #[test]
    fn off_blocks_every_action() {
        let mut g = Governor::default();
        for c in [DeletePod, RestartWorkload, DrainNode] {
            assert_eq!(g.evaluate(&c, false, false, Instant::now()), Verdict::Blocked("switch-off".into()));
        }
    }
    #[test]
    fn on_allows_one_action_per_gap() {
        let mut g = Governor::default();
        let t0 = Instant::now();
        assert_eq!(g.evaluate(&DeletePod, true, false, t0), Verdict::Allowed);
        assert_eq!(g.evaluate(&DrainNode, true, false, t0 + Duration::from_secs(10)), Verdict::Blocked("cooldown".into()));
        assert_eq!(g.evaluate(&DrainNode, true, false, t0 + ACTION_GAP), Verdict::Allowed);
    }
    #[test]
    fn brain_failure_blocks_everything() {
        let mut g = Governor::default();
        assert_eq!(g.evaluate(&DeletePod, true, true, Instant::now()), Verdict::Blocked("brain-failure".into()));
        assert_eq!(g.evaluate(&RaiseAlert { reason: "x".into() }, true, true, Instant::now()), Verdict::Blocked("brain-failure".into()));
    }
    #[test]
    fn alerts_work_when_off_but_are_rate_limited() {
        let mut g = Governor::default();
        let t0 = Instant::now();
        let a = RaiseAlert { reason: "x".into() };
        assert_eq!(g.evaluate(&a, false, false, t0), Verdict::Allowed);
        assert_eq!(g.evaluate(&a, false, false, t0 + Duration::from_secs(5)), Verdict::Blocked("alert-rate-limit".into()));
        assert_eq!(g.evaluate(&NoOp, true, false, t0), Verdict::Blocked("no-op".into()));
    }
}

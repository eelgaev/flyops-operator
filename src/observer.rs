//! Cluster observer: reflector-backed caches of status-bearing objects -> normalised signals.
//! Only status fields are read; labels, annotations, env, and secrets are never touched.
use crate::encoding::{normalize_count, Signals};
use futures::StreamExt;
use k8s_openapi::api::{apps::v1::Deployment, batch::v1::Job, core::v1::{Node, Pod}};
use kube::{
    runtime::{reflector, reflector::Store, watcher, WatchStreamExt},
    Api, Client, ResourceExt,
};
use std::collections::BTreeMap;

/// Every signal the observer produces.
#[cfg(test)]
pub const SIGNALS: &[&str] = &["node_not_ready", "node_memory_pressure", "pod_crash_loop", "pod_pending_duration", "deployment_degraded", "job_failed"];

/// Human-facing counts for the dashboard (no names, no payloads).
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct ObservedCounts {
    pub nodes: usize,
    pub pods: usize,
    pub deployments: usize,
    pub jobs: usize,
}

pub struct Observer {
    nodes: Store<Node>,
    pods: Store<Pod>,
    deployments: Store<Deployment>,
    jobs: Store<Job>,
    /// FlyOps' own namespace: the fly can't act there, so it doesn't feel it either.
    own_ns: String,
}

// ------------------------------------------------------------ pure functions

/// Count at which a count-based signal saturates at 1.0. One broken pod/Deployment reads 0.2, enough to clear the
/// encoder's 0.15 noise threshold, so a single failure is never invisible.
const FULL_SCALE: usize = 5;

pub fn node_condition(n: &Node, ty: &str) -> Option<String> {
    n.status.as_ref()?.conditions.as_ref()?.iter().find(|c| c.type_ == ty).map(|c| c.status.clone())
}

pub fn node_not_ready(nodes: &[std::sync::Arc<Node>]) -> Option<f32> {
    if nodes.is_empty() { return None; }
    // A dead node or stopped kubelet shows Ready=Unknown (NodeStatusUnknown), not False; a missing condition is
    // not healthy either. Only an explicit Ready=True counts as ready.
    let bad = nodes.iter().filter(|n| node_condition(n, "Ready").as_deref() != Some("True")).count();
    Some(bad as f32 / nodes.len() as f32)
}

pub fn node_memory_pressure(nodes: &[std::sync::Arc<Node>]) -> Option<f32> {
    if nodes.is_empty() { return None; }
    let bad = nodes.iter().filter(|n| node_condition(n, "MemoryPressure").as_deref() == Some("True")).count();
    Some(bad as f32 / nodes.len() as f32)
}

/// Restarts at which a not-ready container that keeps exiting non-zero counts as crash-looping even while it is
/// momentarily `terminated`/`running` between back-offs rather than `waiting: CrashLoopBackOff`.
const CRASH_RESTARTS: i32 = 2;

pub fn is_crash_looping(p: &Pod) -> bool {
    p.status.as_ref().and_then(|s| s.container_statuses.as_ref()).is_some_and(|cs| {
        cs.iter().any(|c| {
            let st = c.state.as_ref();
            let backoff = st.and_then(|s| s.waiting.as_ref()).and_then(|w| w.reason.as_deref()) == Some("CrashLoopBackOff");
            let failed_now = st.and_then(|s| s.terminated.as_ref()).is_some_and(|t| t.exit_code != 0);
            let failed_last = c.last_state.as_ref().and_then(|s| s.terminated.as_ref()).is_some_and(|t| t.exit_code != 0);
            backoff || (!c.ready && c.restart_count >= CRASH_RESTARTS && (failed_now || failed_last))
        })
    })
}

pub fn pod_crash_loop(pods: &[std::sync::Arc<Pod>]) -> f32 {
    normalize_count(pods.iter().filter(|p| is_crash_looping(p)).count(), FULL_SCALE)
}

/// Pending pods, weighted by age: full scale at 10 pods pending for >=10 minutes.
pub fn pod_pending(pods: &[std::sync::Arc<Pod>], now: chrono::DateTime<chrono::Utc>) -> f32 {
    let score: f32 = pods
        .iter()
        .filter(|p| p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Pending"))
        .map(|p| {
            let age = p.creation_timestamp().map(|t| (now.timestamp() - t.0.as_second()).max(0) as f32).unwrap_or(0.0);
            (age / 600.0).min(1.0)
        })
        .sum();
    (score / 10.0).clamp(0.0, 1.0)
}

pub fn deployment_degraded(ds: &[std::sync::Arc<Deployment>]) -> f32 {
    let bad = ds
        .iter()
        .filter(|d| {
            let want = d.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
            let avail = d.status.as_ref().and_then(|s| s.available_replicas).unwrap_or(0);
            want > 0 && avail < want
        })
        .count();
    normalize_count(bad, FULL_SCALE)
}

pub fn job_failed(jobs: &[std::sync::Arc<Job>]) -> f32 {
    normalize_count(jobs.iter().filter(|j| j.status.as_ref().and_then(|s| s.failed).unwrap_or(0) > 0 && j.status.as_ref().and_then(|s| s.succeeded).unwrap_or(0) == 0).count(), 5)
}

// ------------------------------------------------------------------ runtime

impl Observer {
    /// Starts background watches. Watch errors are logged and retried with backoff.
    pub fn start(client: Client, own_ns: &str) -> Self {
        fn spawn<K>(client: Client) -> Store<K>
        where
            K: kube::Resource<DynamicType = ()> + Clone + std::fmt::Debug + serde::de::DeserializeOwned + Send + Sync + 'static,
        {
            let (store, writer) = reflector::store::<K>();
            let api: Api<K> = Api::all(client);
            tokio::spawn(async move {
                let stream = reflector(writer, watcher(api, watcher::Config::default()))
                    .default_backoff()
                    .applied_objects();
                futures::pin_mut!(stream);
                while let Some(ev) = stream.next().await {
                    if let Err(e) = ev {
                        tracing::warn!(error = %e, "watch error");
                    }
                }
            });
            store
        }
        Self { nodes: spawn(client.clone()), pods: spawn(client.clone()), deployments: spawn(client.clone()), jobs: spawn(client), own_ns: own_ns.to_string() }
    }

    fn outside<K: kube::Resource>(&self, items: Vec<std::sync::Arc<K>>) -> Vec<std::sync::Arc<K>> {
        items.into_iter().filter(|o| o.meta().namespace.as_deref() != Some(self.own_ns.as_str())).collect()
    }

    /// Current cache contents, for picking action targets without extra API calls.
    pub fn pods(&self) -> Vec<std::sync::Arc<Pod>> { self.pods.state() }
    pub fn nodes(&self) -> Vec<std::sync::Arc<Node>> { self.nodes.state() }

    pub fn counts(&self) -> ObservedCounts {
        ObservedCounts { nodes: self.nodes.state().len(), pods: self.pods.state().len(), deployments: self.deployments.state().len(), jobs: self.jobs.state().len() }
    }

    /// Normalised snapshot. Signals whose source is unknown are omitted (never zero).
    pub fn signals(&self, now: chrono::DateTime<chrono::Utc>) -> Signals {
        let mut s: BTreeMap<String, f32> = BTreeMap::new();
        let nodes = self.nodes.state();
        if let Some(v) = node_not_ready(&nodes) { s.insert("node_not_ready".into(), v); }
        if let Some(v) = node_memory_pressure(&nodes) { s.insert("node_memory_pressure".into(), v); }
        let pods = self.outside(self.pods.state());
        s.insert("pod_crash_loop".into(), pod_crash_loop(&pods));
        s.insert("pod_pending_duration".into(), pod_pending(&pods, now));
        s.insert("deployment_degraded".into(), deployment_degraded(&self.outside(self.deployments.state())));
        s.insert("job_failed".into(), job_failed(&self.outside(self.jobs.state())));
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{ContainerState, ContainerStateWaiting, ContainerStatus, NodeCondition, NodeStatus, PodStatus};
    use std::sync::Arc;

    fn node(ready: &str, mem: &str) -> Arc<Node> {
        let c = |t: &str, s: &str| NodeCondition { type_: t.into(), status: s.into(), ..Default::default() };
        Arc::new(Node { status: Some(NodeStatus { conditions: Some(vec![c("Ready", ready), c("MemoryPressure", mem)]), ..Default::default() }), ..Default::default() })
    }
    fn pod(reason: Option<&str>, phase: &str) -> Arc<Pod> {
        let cs = reason.map(|r| vec![ContainerStatus { state: Some(ContainerState { waiting: Some(ContainerStateWaiting { reason: Some(r.into()), ..Default::default() }), ..Default::default() }), ..Default::default() }]);
        Arc::new(Pod { status: Some(PodStatus { phase: Some(phase.into()), container_statuses: cs, ..Default::default() }), ..Default::default() })
    }

    #[test]
    fn node_fractions() {
        let ns = vec![node("True", "False"), node("False", "True")];
        assert_eq!(node_not_ready(&ns), Some(0.5));
        assert_eq!(node_memory_pressure(&ns), Some(0.5));
        assert_eq!(node_not_ready(&[]), None, "no nodes => unknown, not zero");
        // stopped kubelet / dead node: the node controller sets Ready=Unknown
        assert_eq!(node_not_ready(&[node("True", "False"), node("Unknown", "Unknown")]), Some(0.5));
    }
    #[test]
    fn crash_loop_detected() {
        let ps = vec![pod(Some("CrashLoopBackOff"), "Running"), pod(None, "Running")];
        assert_eq!(pod_crash_loop(&ps), 0.2);
    }
    #[test]
    fn crash_loop_detected_between_backoffs() {
        use k8s_openapi::api::core::v1::ContainerStateTerminated;
        let term = |code| Some(ContainerState { terminated: Some(ContainerStateTerminated { exit_code: code, ..Default::default() }), ..Default::default() });
        let p = |restarts, ready, state, last| Arc::new(Pod { status: Some(PodStatus { container_statuses: Some(vec![ContainerStatus { restart_count: restarts, ready, state, last_state: last, ..Default::default() }]), ..Default::default() }), ..Default::default() });
        assert!(is_crash_looping(&p(3, false, term(1), term(1))), "terminated:Error between back-offs");
        assert!(!is_crash_looping(&p(1, false, term(1), None)), "a single failure is not a loop");
        assert!(!is_crash_looping(&p(5, true, None, term(1))), "ready again => recovered");
        assert!(!is_crash_looping(&p(5, false, term(0), term(0))), "clean exits are not crashes");
    }
    #[test]
    fn pending_without_timestamp_scores_zero() {
        assert_eq!(pod_pending(&[pod(None, "Pending")], chrono::Utc::now()), 0.0);
    }
}

//! The fly's three actions. Each works on the whole cluster and picks its own target from what is unhealthy.
//! The brain only chooses *which* action; target selection here is deterministic.
//!
//! Never touched: FlyOps' own namespace (so the OFF switch always keeps working), pods without a controller
//! (deleting them would lose them for good), DaemonSet and static pods during a drain.
use k8s_openapi::api::{apps::v1::{DaemonSet, Deployment, StatefulSet}, core::v1::{Node, Pod}};
use kube::{api::{DeleteParams, EvictParams, ListParams, Patch, PatchParams, PostParams, Preconditions}, Api, Client, ResourceExt};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("kube: {0}")]
    Kube(#[from] kube::Error),
}

// ------------------------------------------------------------------ health

/// Unhealthy = failed, crash-looping / image-pull failing, or running but not ready.
pub fn pod_unhealthy(p: &Pod) -> bool {
    let Some(st) = p.status.as_ref() else { return false };
    if st.phase.as_deref() == Some("Failed") { return true; }
    let bad_wait = st.container_statuses.iter().flatten().any(|c| {
        matches!(c.state.as_ref().and_then(|s| s.waiting.as_ref()).and_then(|w| w.reason.as_deref()),
                 Some("CrashLoopBackOff" | "ImagePullBackOff" | "ErrImagePull" | "CreateContainerConfigError"))
    });
    let not_ready = st.phase.as_deref() == Some("Running")
        && st.conditions.iter().flatten().any(|c| c.type_ == "Ready" && c.status == "False");
    bad_wait || not_ready
}

fn restarts(p: &Pod) -> i32 {
    p.status.as_ref().and_then(|s| s.container_statuses.as_ref()).map(|cs| cs.iter().map(|c| c.restart_count).sum()).unwrap_or(0)
}

fn is_mirror(p: &Pod) -> bool {
    p.metadata.annotations.as_ref().is_some_and(|a| a.contains_key("kubernetes.io/config.mirror"))
}

fn controller_owner(p: &Pod) -> Option<&k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference> {
    p.metadata.owner_references.iter().flatten().find(|o| o.controller == Some(true))
}

/// A pod the fly may act on: someone else's, controller-owned, not static, not already going away.
fn actionable(p: &Pod, own_ns: &str) -> bool {
    p.metadata.namespace.as_deref().is_some_and(|ns| ns != own_ns)
        && p.metadata.deletion_timestamp.is_none()
        && !is_mirror(p)
        && controller_owner(p).is_some_and(|o| o.kind != "Node")
}

fn pod_ref(p: &Pod) -> String { format!("{}/{}", p.namespace().unwrap_or_default(), p.name_any()) }

// ------------------------------------------------------------------ delete-pod

/// The worst unhealthy pod in the cluster (most restarts; ties broken by namespace/name).
pub fn pick_pod<'a>(pods: &'a [Arc<Pod>], own_ns: &str) -> Option<&'a Pod> {
    pods.iter().map(|p| p.as_ref())
        .filter(|p| actionable(p, own_ns) && pod_unhealthy(p))
        .max_by(|a, b| restarts(a).cmp(&restarts(b)).then_with(|| pod_ref(b).cmp(&pod_ref(a))))
}

pub async fn delete_pod(client: Client, pods: &[Arc<Pod>], own_ns: &str) -> Result<String, ExecError> {
    let Some(p) = pick_pod(pods, own_ns) else { return Ok("nothing to do: no unhealthy pod".into()) };
    let api: Api<Pod> = Api::namespaced(client, &p.namespace().unwrap_or_default());
    let dp = DeleteParams { preconditions: Some(Preconditions { uid: p.metadata.uid.clone(), resource_version: None }), ..Default::default() };
    api.delete(&p.name_any(), &dp).await?;
    Ok(format!("deleted pod {} (restarts {})", pod_ref(p), restarts(p)))
}

// ------------------------------------------------------------------ restart-workload

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Workload {
    pub kind: &'static str,
    pub namespace: String,
    pub name: String,
}

/// The Deployment, StatefulSet or DaemonSet a pod belongs to. A Deployment's pods are owned by a ReplicaSet named
/// `<deployment>-<pod-template-hash>`, so the Deployment name is recovered from that.
pub fn workload_of(p: &Pod) -> Option<Workload> {
    let o = controller_owner(p)?;
    let namespace = p.namespace()?;
    let (kind, name) = match o.kind.as_str() {
        "StatefulSet" => ("StatefulSet", o.name.clone()),
        "DaemonSet" => ("DaemonSet", o.name.clone()),
        "ReplicaSet" => {
            let hash = p.labels().get("pod-template-hash")?;
            ("Deployment", o.name.strip_suffix(&format!("-{hash}"))?.to_string())
        }
        _ => return None,
    };
    Some(Workload { kind, namespace, name })
}

/// The workload with the most unhealthy pods (ties broken by kind/namespace/name).
pub fn pick_workload(pods: &[Arc<Pod>], own_ns: &str) -> Option<(Workload, usize)> {
    let mut counts: BTreeMap<Workload, usize> = BTreeMap::new();
    for p in pods.iter().filter(|p| actionable(p, own_ns) && pod_unhealthy(p)) {
        if let Some(w) = workload_of(p) { *counts.entry(w).or_default() += 1; }
    }
    counts.into_iter().max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
}

/// Same as `oc rollout restart`: bump a pod-template annotation so the controller rolls every pod.
pub async fn restart_workload(client: Client, pods: &[Arc<Pod>], own_ns: &str) -> Result<String, ExecError> {
    let Some((w, n)) = pick_workload(pods, own_ns) else { return Ok("nothing to do: no workload with unhealthy pods".into()) };
    let patch = serde_json::json!({"spec": {"template": {"metadata": {"annotations": {
        "kubectl.kubernetes.io/restartedAt": chrono::Utc::now().to_rfc3339() }}}}});
    let (pp, mp) = (PatchParams::default(), Patch::Merge(&patch));
    match w.kind {
        "Deployment" => { Api::<Deployment>::namespaced(client, &w.namespace).patch(&w.name, &pp, &mp).await?; }
        "StatefulSet" => { Api::<StatefulSet>::namespaced(client, &w.namespace).patch(&w.name, &pp, &mp).await?; }
        _ => { Api::<DaemonSet>::namespaced(client, &w.namespace).patch(&w.name, &pp, &mp).await?; }
    }
    Ok(format!("restarted {} {}/{} ({n} unhealthy pods)", w.kind.to_lowercase(), w.namespace, w.name))
}

// ------------------------------------------------------------------ drain-node

fn node_cond(n: &Node, ty: &str) -> Option<String> {
    n.status.as_ref()?.conditions.as_ref()?.iter().find(|c| c.type_ == ty).map(|c| c.status.clone())
}
fn node_ready(n: &Node) -> bool { node_cond(n, "Ready").as_deref() == Some("True") }
fn schedulable(n: &Node) -> bool { !n.spec.as_ref().and_then(|s| s.unschedulable).unwrap_or(false) }
fn node_unhealthy(n: &Node) -> bool {
    !node_ready(n) || ["MemoryPressure", "DiskPressure", "PIDPressure"].iter().any(|c| node_cond(n, c).as_deref() == Some("True"))
}

/// A node that is down or under pressure and not cordoned yet, provided another Ready, schedulable node is left
/// to take its pods. Deterministic (lowest name).
pub fn pick_node(nodes: &[Arc<Node>]) -> Option<&Node> {
    let mut cands: Vec<&Node> = nodes.iter().map(|n| n.as_ref()).filter(|n| schedulable(n) && node_unhealthy(n)).collect();
    cands.sort_by_key(|n| n.name_any());
    cands.into_iter().find(|c| nodes.iter().any(|n| n.name_any() != c.name_any() && node_ready(n) && schedulable(n)))
}

/// Evicted during a drain: everything except DaemonSet pods, static pods, finished pods and FlyOps itself.
pub fn evictable(p: &Pod, own_ns: &str) -> bool {
    p.metadata.deletion_timestamp.is_none()
        && p.metadata.namespace.as_deref() != Some(own_ns)
        && !is_mirror(p)
        && !p.metadata.owner_references.iter().flatten().any(|o| o.kind == "DaemonSet")
        && !matches!(p.status.as_ref().and_then(|s| s.phase.as_deref()), Some("Succeeded" | "Failed"))
}

/// Cordon, then evict through the Eviction API so PodDisruptionBudgets are honoured.
pub async fn drain_node(client: Client, nodes: &[Arc<Node>], own_ns: &str) -> Result<String, ExecError> {
    let Some(node) = pick_node(nodes) else { return Ok("nothing to do: no unhealthy node (or it is the last one)".into()) };
    let name = node.name_any();
    let nodes_api: Api<Node> = Api::all(client.clone());
    nodes_api.patch(&name, &PatchParams::default(), &Patch::Merge(&serde_json::json!({"spec": {"unschedulable": true}}))).await?;
    let pods = Api::<Pod>::all(client.clone()).list(&ListParams::default().fields(&format!("spec.nodeName={name}"))).await?.items;
    let (mut evicted, mut blocked) = (0usize, 0usize);
    for p in pods.iter().filter(|p| evictable(p, own_ns)) {
        let api: Api<Pod> = Api::namespaced(client.clone(), &p.namespace().unwrap_or_default());
        let ep = EvictParams {
            delete_options: Some(DeleteParams { preconditions: Some(Preconditions { uid: p.metadata.uid.clone(), resource_version: None }), ..Default::default() }),
            post_options: PostParams::default(),
        };
        match api.evict(&p.name_any(), &ep).await {
            Ok(_) => evicted += 1,
            Err(kube::Error::Api(e)) if [404, 409, 429].contains(&e.code) => blocked += 1, // gone / conflict / PodDisruptionBudget
            Err(e) => return Err(e.into()),
        }
    }
    Ok(format!("drained node {name}: evicted {evicted}, blocked {blocked} (undo with: oc adm uncordon {name})"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{ContainerState, ContainerStateWaiting, ContainerStatus, NodeCondition, NodeSpec, NodeStatus, PodStatus};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;

    const OWN: &str = "flyops-system";

    fn pod(ns: &str, name: &str, owner: Option<(&str, &str)>, crash: bool, restarts: i32) -> Arc<Pod> {
        let mut p = Pod::default();
        p.metadata.namespace = Some(ns.into());
        p.metadata.name = Some(name.into());
        p.metadata.labels = Some([("pod-template-hash".to_string(), "abc12".to_string())].into());
        if let Some((kind, oname)) = owner {
            p.metadata.owner_references = Some(vec![OwnerReference { controller: Some(true), kind: kind.into(), name: oname.into(), ..Default::default() }]);
        }
        let state = crash.then(|| ContainerState { waiting: Some(ContainerStateWaiting { reason: Some("CrashLoopBackOff".into()), ..Default::default() }), ..Default::default() });
        p.status = Some(PodStatus { phase: Some("Running".into()), container_statuses: Some(vec![ContainerStatus { restart_count: restarts, state, ..Default::default() }]), ..Default::default() });
        Arc::new(p)
    }
    fn node(name: &str, ready: bool, cordoned: bool) -> Arc<Node> {
        let mut n = Node { status: Some(NodeStatus { conditions: Some(vec![NodeCondition { type_: "Ready".into(), status: if ready { "True" } else { "Unknown" }.into(), ..Default::default() }]), ..Default::default() }),
            spec: Some(NodeSpec { unschedulable: Some(cordoned), ..Default::default() }), ..Default::default() };
        n.metadata.name = Some(name.into());
        Arc::new(n)
    }

    #[test]
    fn delete_pod_picks_worst_unhealthy_anywhere_except_flyops() {
        let rs = Some(("ReplicaSet", "web-abc12"));
        let pods = vec![
            pod("shop", "ok", rs, false, 0),
            pod("shop", "bad", rs, true, 3),
            pod("kube-system", "worse", Some(("DaemonSet", "dns")), true, 9),
            pod(OWN, "self", rs, true, 99),
            pod("shop", "naked", None, true, 50),
        ];
        assert_eq!(pick_pod(&pods, OWN).unwrap().name_any(), "worse", "system namespaces are fair game");
        assert!(pick_pod(&pods[..1], OWN).is_none(), "healthy pods are never deleted");
        assert!(pick_pod(&pods[3..], OWN).is_none(), "never FlyOps itself, never pods without a controller");
    }

    #[test]
    fn workload_resolution_and_choice() {
        let p = pod("shop", "web-abc12-x", Some(("ReplicaSet", "web-abc12")), true, 1);
        assert_eq!(workload_of(&p), Some(Workload { kind: "Deployment", namespace: "shop".into(), name: "web".into() }));
        let ds = pod("kube-system", "dns-x", Some(("DaemonSet", "dns")), true, 1);
        assert_eq!(workload_of(&ds).unwrap().kind, "DaemonSet");
        let pods = vec![p.clone(), pod("shop", "web-abc12-y", Some(("ReplicaSet", "web-abc12")), true, 1), ds,
                        pod(OWN, "self", Some(("ReplicaSet", "flyops-abc12")), true, 1)];
        let (w, n) = pick_workload(&pods, OWN).unwrap();
        assert_eq!((w.name.as_str(), n), ("web", 2));
    }

    #[test]
    fn drain_picks_a_down_node_but_never_the_last_one() {
        assert_eq!(pick_node(&[node("a", true, false), node("b", false, false)]).unwrap().name_any(), "b");
        assert!(pick_node(&[node("a", false, false)]).is_none(), "the only node is never drained");
        assert!(pick_node(&[node("a", true, false), node("b", false, true)]).is_none(), "already cordoned");
        assert!(pick_node(&[node("a", false, false), node("b", false, false)]).is_none(), "no Ready node left to take the pods");
    }

    #[test]
    fn eviction_skips() {
        let rs = Some(("ReplicaSet", "web-abc12"));
        assert!(evictable(&pod("shop", "a", rs, false, 0), OWN));
        assert!(!evictable(&pod(OWN, "a", rs, false, 0), OWN));
        assert!(!evictable(&pod("shop", "a", Some(("DaemonSet", "x")), false, 0), OWN));
    }
}

use crate::{
    election::{LeaderState, start},
    error::{Error, Result},
    ext::SecretExt,
    labels::*,
    metrics::MetricState,
    whisper::Whisper,
};
use futures::StreamExt;
use k8s_openapi::api::core::v1::{Namespace, ObjectReference, Secret};
use kube::{
    Api, Client, Resource, ResourceExt,
    api::{DeleteParams, ListParams, Patch, PatchParams, Preconditions},
    runtime::{
        Controller,
        controller::{Action, Config as ControllerConfig, Error as RuntimeError},
        events::{Event as Notice, EventType, Recorder, Reporter},
        finalizer::{Event, finalizer},
        reflector::ObjectRef,
        watcher::Config,
    },
};
use serde_json::json;
use std::{
    collections::HashSet,
    env,
    sync::{Arc, OnceLock},
};
use tokio::time::Duration;
use tracing::{info, instrument, warn};

struct Context {
    client: Client,
    recorder: Recorder,
    metrics: MetricState,
    state: LeaderState,
    /// Namespaces the operator may write copies into (the chart's
    /// `writeNamespaces`, passed via `WRITE_NAMESPACES`). RBAC confines copies to
    /// exactly this set, so probing it by name is a complete way to rediscover
    /// orphans when status is lost — no cluster-wide secret list. See ADR 0003.
    write_namespaces: NSSet,
    /// Namespaces being decommissioned (`drainNamespaces` / `DRAIN_NAMESPACES`).
    /// The operator keeps probing and reclaiming copies here but never writes new
    /// ones — so a namespace can be removed from active sync without stranding the
    /// copies already in it. Always disjoint from `write_namespaces`.
    drain_namespaces: NSSet,
}

impl Context {
    /// Namespaces to name-probe for existing copies: everywhere the operator can
    /// still `get` a secret — active write targets plus draining namespaces.
    fn probe_namespaces(&self) -> impl Iterator<Item = &String> {
        self.write_namespaces.union(&self.drain_namespaces)
    }

    async fn record(&self, notice: &Notice, reference: &ObjectReference) -> Result<()> {
        self.recorder
            .publish(notice, reference)
            .await
            .map_err(Error::Event)
    }
}

type NSSet = HashSet<String>;

/// Namespaces that may never receive synced secrets, regardless of labels.
/// Writing into these could let a tenant tamper with cluster-critical secrets.
const PROTECTED_NAMESPACES: &[&str] = &["kube-system", "kube-public", "kube-node-lease"];

/// The operator's own namespace, set once at startup from the in-cluster
/// ServiceAccount (`client.default_namespace()`). Storing it here means the
/// protected-namespace check never depends on the optional `CONTROLLER_NAMESPACE`
/// env var being set — an unset env fails *closed*, not open.
static OPERATOR_NAMESPACE: OnceLock<String> = OnceLock::new();

/// The namespace to treat as the operator's own. Prefer the value derived from
/// the client at startup; fall back to the `CONTROLLER_NAMESPACE` env only if that
/// wasn't set (e.g. in unit tests that call the check directly).
fn operator_namespace() -> Option<String> {
    OPERATOR_NAMESPACE.get().cloned().or_else(|| {
        env::var("CONTROLLER_NAMESPACE")
            .ok()
            .filter(|s| !s.is_empty())
    })
}

/// True if `ns` must never be used as a sync target. Covers the well-known
/// system namespaces plus the operator's own namespace, so a tenant can't target
/// the controller (or the namespace holding everyone's source secrets).
fn is_protected_namespace(ns: &str) -> bool {
    PROTECTED_NAMESPACES.contains(&ns) || operator_namespace().as_deref() == Some(ns)
}

/// The single predicate every reclaim path ([`discover_copies`], [`delete_copy`])
/// uses to decide a secret is a copy this Whisper owns and may delete. Two checks,
/// applied together so a caller can't accidentally use only half:
///
/// 1. **Attribution** — the `owner-uid` label matches this Whisper's UID. This is
///    the strong check (done first to short-circuit the common rejection): the UID
///    is server-assigned, immutable, and a random UUID, so unlike the markers
///    below a tenant can't set a *matching* value without first *learning* it
///    (reading the Whisper or an existing copy). An empty `owner_uid` never matches
///    — a Whisper from the API always has one, so empty means a degenerate object.
///    Because the UID is immutable, attribution also survives `spec.secretName`
///    renames.
/// 2. **Markers** — the `whisper` label and our `managedFields` field manager, as
///    cheap defense-in-depth. These are public constants a namespace writer can
///    forge, so they're not a boundary; they just ensure we never touch a secret
///    that isn't marked as a copy at all.
///
/// Neither check is an authorization boundary on its own — that is RBAC (deletes
/// are confined to `writeNamespaces`/`drainNamespaces`), the delete preconditions
/// in [`delete`], and the assumption those namespaces aren't tenant-writable. See
/// ADR 0001/0003.
fn is_owned_copy(secret: &Secret, owner_uid: &str) -> bool {
    let labels = secret.labels();
    // 1. Attribution by the capability-gated owner UID.
    let attributed = !owner_uid.is_empty()
        && labels
            .get(OWNER_UID_LABEL)
            .map(|v| v == owner_uid)
            .unwrap_or(false);
    // 2. Copy markers: the whisper label AND our SSA field manager.
    let marked = labels
        .get(WHISPER_LABEL)
        .map(|v| v == "true")
        .unwrap_or(false);
    let ours = secret.meta().managed_fields.as_ref().is_some_and(|fields| {
        fields
            .iter()
            .any(|f| f.manager.as_deref() == Some(FIELD_MANAGER))
    });
    attributed && marked && ours
}

/// Rediscover where copies named `copy_name` (owned by the Whisper with UID
/// `owner_uid`) currently live, by probing each configured `writeNamespaces`
/// namespace with a `get` — never a `list`.
///
/// This is what lets a lost `status.syncedNamespaces` self-heal without
/// cluster-wide secret read. Two facts make a name-probe complete: copy names are
/// stable (`SecretExt::dup` names a copy after the Whisper, not the mutable
/// `secretName`), and RBAC physically confines copies to `writeNamespaces`, so no
/// copy can exist outside the set we scan. Using `get` (which the operator
/// already holds for these namespaces) rather than `list` deliberately avoids
/// granting enumeration — see ADR 0003's threat model.
async fn discover_copies(copy_name: &str, owner_uid: &str, ctx: &Arc<Context>) -> Result<NSSet> {
    let mut found = NSSet::new();
    for ns in ctx.probe_namespaces() {
        let api: Api<Secret> = Api::namespaced(ctx.client.clone(), ns);
        if let Some(secret) = api.get_opt(copy_name).await.map_err(Error::GetSecret)?
            && is_owned_copy(&secret, owner_uid)
        {
            found.insert(ns.clone());
        }
    }
    Ok(found)
}

/// True if a target namespace has consented to receiving synced secrets, i.e.
/// it is not protected and carries `whisperer.jeffl.es/allow-sync=true`. This
/// consent check is the authorization boundary that stops a secret author from
/// copying data into namespaces they don't control.
fn is_consenting_namespace(ns: &Namespace) -> bool {
    !is_protected_namespace(&ns.name_any())
        && ns
            .labels()
            .get(ALLOW_SYNC_LABEL)
            .map(|v| v == "true")
            .unwrap_or(false)
}

/// Outcome of matching a secret's requested namespaces against what the cluster
/// actually offers. `targets` are the namespaces we will sync into; `unknown`
/// (don't exist) and `refused` (exist but haven't opted in) are kept only so
/// the caller can log them.
struct TargetResolution {
    targets: NSSet,
    unknown: Vec<String>,
    refused: Vec<String>,
}

/// Pure core of [`secret_namespaces`]: given the `wanted` namespaces from the
/// annotation and the cluster's current namespaces, decide which are valid
/// targets. Split out from the API call so it can be unit-tested without a
/// cluster. A namespace is a target only if it is `wanted`, exists, and
/// consents (see [`is_consenting_namespace`]).
fn resolve_targets(wanted: &NSSet, namespaces: &[Namespace]) -> TargetResolution {
    let existing = namespaces.iter().map(|ns| ns.name_any()).collect::<NSSet>();
    let consenting = namespaces
        .iter()
        .filter(|ns| is_consenting_namespace(ns))
        .map(|ns| ns.name_any())
        .collect::<NSSet>();

    let unknown = wanted.difference(&existing).cloned().collect::<Vec<_>>();
    // Requested namespaces that exist but have not opted in are refused — this
    // is where a cross-tenant injection attempt gets dropped.
    let refused = wanted
        .iter()
        .filter(|n| existing.contains(*n) && !consenting.contains(*n))
        .cloned()
        .collect::<Vec<_>>();
    let targets = wanted.intersection(&consenting).cloned().collect::<NSSet>();

    TargetResolution {
        targets,
        unknown,
        refused,
    }
}

/// List the cluster's namespaces and resolve `wanted` down to the namespaces we
/// will actually sync into, logging any that don't exist or haven't opted in.
///
/// A namespace is a target only if it exists, consents
/// (`whisperer.jeffl.es/allow-sync=true`) and isn't protected — the authorization
/// boundary — *and* isn't being decommissioned (`drain`). Draining namespaces are
/// excluded here, alongside the other never-a-target rules, so every caller of
/// `resolve` gets real targets without re-applying the exclusion.
async fn resolve(wanted: &NSSet, drain: &NSSet, client: &Client) -> Result<NSSet, Error> {
    let all = Api::<Namespace>::all(client.clone())
        .list(&ListParams::default())
        .await
        .map_err(Error::ListNamespaces)?;

    let resolution = resolve_targets(wanted, &all.items);
    if !resolution.unknown.is_empty() {
        warn!(
            "requested namespaces '{}' do not exist",
            resolution.unknown.join(",")
        );
    }
    if !resolution.refused.is_empty() {
        warn!(
            "requested namespaces '{}' have not opted in via {ALLOW_SYNC_LABEL}=true; refusing to sync there",
            resolution.refused.join(",")
        );
    }

    Ok(resolution.targets.difference(drain).cloned().collect())
}

/// What a single reconcile should do to the cluster: write a copy into every
/// namespace in `to_write`, and reclaim the copy from every namespace in
/// `to_delete`. The two sets are always disjoint.
struct SyncPlan {
    to_write: NSSet,
    to_delete: NSSet,
}

/// Pure reconcile planner. Given where copies currently live (`synced`, read
/// from `status.syncedNamespaces`) and the resolved consenting `targets`, decide
/// what to write and what to reclaim:
///
/// - `to_write` is every current target — writes are idempotent server-side
///   applies, so re-writing an existing copy is a no-op refresh.
/// - `to_delete` is every namespace we synced into before that is no longer a
///   target. Because it comes from status, cleanup never needs a cluster-wide
///   secret `list` to rediscover orphans.
///
/// Two invariants hold for all inputs (see the proptests): the sets are disjoint
/// (`to_write ∩ to_delete == ∅`, so we never delete a namespace we're about to
/// write), and applying the plan converges the live copy set to exactly
/// `targets` (`(synced ∪ to_write) \ to_delete == targets`).
fn plan(synced: &NSSet, targets: &NSSet) -> SyncPlan {
    SyncPlan {
        to_write: targets.clone(),
        to_delete: synced.difference(targets).cloned().collect(),
    }
}

/// Reconcile one Whisper: read its source secret, resolve the consenting
/// targets, reclaim copies in namespaces that fell out of the target set
/// (discovered from the status record plus a name-probe of the write namespaces,
/// so no cluster-wide list), write copies into the current targets, and record
/// where they now live.
#[instrument(skip(ctx, whisper))]
async fn apply(whisper: Arc<Whisper>, ctx: Arc<Context>) -> Result<Action, Error> {
    let client = &ctx.client;
    let namespace = whisper.namespace().unwrap_or_default();
    let secret_name = whisper.spec.secret_name.clone();
    // Copies are named after the Whisper (stable across secretName renames) and
    // tagged with its UID so we can always find and attribute them.
    let copy_name = whisper.name_any();
    let owner_uid = whisper.uid().unwrap_or_default();
    info!("reconciling whisper {copy_name} in {namespace} (source secret {secret_name})");

    // The source secret lives in this Whisper's own namespace.
    let src: Api<Secret> = Api::namespaced(client.clone(), &namespace);
    let secret = match src.get_opt(&secret_name).await.map_err(Error::GetSecret)? {
        Some(secret) => secret,
        None => {
            warn!("secret '{secret_name}' not found in '{namespace}'; will retry");
            return Ok(Action::requeue(Duration::from_secs(30)));
        }
    };

    let wanted = whisper.spec.namespaces.iter().cloned().collect::<NSSet>();
    // `resolve` excludes draining namespaces, so a target moved to `drainNamespaces`
    // falls out of `targets` and its copy is reclaimed (into `to_delete`) below.
    let targets = resolve(&wanted, &ctx.drain_namespaces, client).await?;

    // Where copies live is the status record UNION what a name-probe of the
    // write namespaces actually finds. Status is the fast path; the probe makes
    // it self-healing — if status was lost, orphans are still found (and so
    // reclaimed below) without any cluster-wide secret list. See ADR 0003.
    let recorded = whisper
        .status
        .as_ref()
        .map(|s| s.synced_namespaces.iter().cloned().collect::<NSSet>())
        .unwrap_or_default();
    let discovered = discover_copies(&copy_name, &owner_uid, &ctx).await?;
    let synced = recorded.union(&discovered).cloned().collect::<NSSet>();

    let SyncPlan {
        to_write,
        to_delete,
    } = plan(&synced, &targets);

    // Orphans are namespaces we synced into before but aren't targets now. We
    // know them from status plus the probe, so we delete by name in those
    // specific namespaces (verifying origin) — never a cluster-wide secret list.
    for ns in &to_delete {
        info!("reclaiming '{copy_name}' from '{ns}' (no longer a target)");
        delete_copy(copy_name.clone(), &owner_uid, ns.clone(), ctx.clone()).await?;
        ctx.record(
            &Notice {
                type_: EventType::Normal,
                reason: "Delete Requested".into(),
                note: Some(format!("Reclaiming '{copy_name}' from '{ns}'")),
                action: "Delete".into(),
                secondary: None,
            },
            &whisper.object_ref(&()),
        )
        .await?;
    }

    // Write a copy into every current target (idempotent server-side apply). The
    // copy is named after the Whisper, so a secretName rename refreshes it in
    // place rather than leaving an old-named orphan.
    for ns in &to_write {
        let api: Api<Secret> = Api::namespaced(client.clone(), ns);
        let copy = secret.dup(&copy_name, &owner_uid, &namespace, ns.clone());
        let res = api
            .patch(
                &copy_name,
                &PatchParams::apply(FIELD_MANAGER),
                &Patch::Apply(&copy),
            )
            .await
            .map_err(Error::Patch)?;
        ctx.record(
            &Notice {
                type_: EventType::Normal,
                reason: "Sync Requested".into(),
                note: Some(format!("Synced '{copy_name}' from '{namespace}' to '{ns}'")),
                action: "Sync".into(),
                secondary: Some(res.object_ref(&())),
            },
            &whisper.object_ref(&()),
        )
        .await?;
        info!("whispered '{copy_name}' (source '{secret_name}') from '{namespace}' to '{ns}'");
    }

    // Record where copies now live so the next reconcile can diff for orphans.
    record_synced(&whisper, &namespace, &targets, client).await?;

    Ok(Action::requeue(Duration::from_secs(600)))
}

/// Patch a Whisper's status with the namespaces its secret currently lives in.
async fn record_synced(
    whisper: &Whisper,
    namespace: &str,
    synced: &NSSet,
    client: &Client,
) -> Result<(), Error> {
    let mut list = synced.iter().cloned().collect::<Vec<_>>();
    list.sort();
    let api: Api<Whisper> = Api::namespaced(client.clone(), namespace);
    let patch = json!({ "status": { "syncedNamespaces": list } });
    api.patch_status(
        &whisper.name_any(),
        &PatchParams::default(),
        &Patch::Merge(&patch),
    )
    .await
    .map_err(Error::PatchStatus)?;
    Ok(())
}

/// Delete the exact secret object we verified. Passing the object's UID and
/// resourceVersion as delete preconditions closes a TOCTOU race: if the secret is
/// swapped or recreated between the origin check in [`delete_copy`] and this
/// delete, the API server rejects it (HTTP 409) instead of removing the
/// substituted object.
async fn delete(secret: &Secret, ctx: Arc<Context>) -> Result<()> {
    let name = secret.name_any();
    let namespace = secret.namespace().unwrap_or_default();
    let api: Api<Secret> = Api::namespaced(ctx.client.clone(), &namespace);
    let params = DeleteParams {
        preconditions: Some(Preconditions {
            uid: secret.uid(),
            resource_version: secret.resource_version(),
        }),
        ..Default::default()
    };
    api.delete(&name, &params)
        .await
        .map_err(Error::Delete)?
        .map_left(|_| info!("deleting secret {name} in {namespace}"))
        .map_right(|_| info!("deleted secret {name} in {namespace}"));
    Ok(())
}

/// Delete a secret only if it is genuinely a copy owned by this Whisper.
///
/// Used for every cross-namespace delete: we fetch the target and confirm it
/// passes [`is_owned_copy`] (attributed to this Whisper by owner UID, and marked
/// as one of our copies) before removing it — so a tenant who plants a look-alike
/// secret can't trick the operator into destroying their data, and one Whisper
/// can't reclaim a different Whisper's copy that happens to share a name. A
/// missing secret is a no-op.
async fn delete_copy(
    name: String,
    owner_uid: &str,
    namespace: String,
    ctx: Arc<Context>,
) -> Result<()> {
    let api: Api<Secret> = Api::namespaced(ctx.client.clone(), &namespace);
    match api.get_opt(&name).await.map_err(Error::GetSecret)? {
        Some(secret) if is_owned_copy(&secret, owner_uid) => delete(&secret, ctx).await,
        Some(_) => {
            warn!("refusing to delete secret {name} in {namespace}: not a copy this Whisper owns");
            Ok(())
        }
        None => Ok(()),
    }
}

/// Finalizer cleanup: a Whisper is being deleted, so reclaim every copy it made
/// — the namespaces in `status.syncedNamespaces`, the current spec targets (in
/// case status lagged), and any copy a name-probe of the write namespaces turns
/// up (in case status was lost). The *source* secret is left alone; it's the
/// user's.
#[instrument(skip(ctx, whisper))]
async fn cleanup(whisper: Arc<Whisper>, ctx: Arc<Context>) -> Result<Action> {
    let copy_name = whisper.name_any();
    let owner_uid = whisper.uid().unwrap_or_default();
    let mut copies = whisper
        .status
        .as_ref()
        .map(|s| s.synced_namespaces.iter().cloned().collect::<NSSet>())
        .unwrap_or_default();
    copies.extend(whisper.spec.namespaces.iter().cloned());
    copies.extend(discover_copies(&copy_name, &owner_uid, &ctx).await?);
    let namespaces = copies.iter().cloned().collect::<Vec<_>>().join(", ");
    ctx.record(
        &Notice {
            type_: EventType::Normal,
            reason: "Delete Requested".into(),
            note: Some(format!("Reclaiming '{copy_name}' from {namespaces}")),
            action: "Delete".into(),
            secondary: None,
        },
        &whisper.object_ref(&()),
    )
    .await?;
    for ns in copies {
        delete_copy(copy_name.clone(), &owner_uid, ns, ctx.clone()).await?;
    }
    Ok(Action::await_change())
}

#[instrument(skip(ctx, whisper))]
async fn dispatcher(whisper: Arc<Whisper>, ctx: Arc<Context>) -> Result<Action> {
    if !ctx.state.is_leader() {
        // Requeue (not await_change): a non-leader must re-check later, because the
        // initial reconcile on startup can run *before* this replica wins the lease.
        // await_change would then park this object until the source secret itself
        // changes again — so consent-label / target-namespace changes would never get
        // re-evaluated (a synced secret could silently never appear). Requeueing makes
        // the object re-reconcile once leadership settles.
        info!("not leader (leader is {}), requeueing", ctx.state.leader());
        return Ok(Action::requeue(Duration::from_secs(30)));
    }
    let metrics = ctx.metrics.clone();
    let namespace = whisper.namespace().unwrap_or_default();
    let _record = metrics.duration();
    let api: Api<Whisper> = Api::namespaced(ctx.client.clone(), &namespace);
    finalizer(&api, FINALIZER, whisper, |event| async {
        metrics.reconcile();
        match event {
            Event::Apply(whisper) => apply(whisper, ctx).await,
            Event::Cleanup(whisper) => cleanup(whisper, ctx).await,
        }
    })
    .await
    .map_err(|e| {
        metrics.failure();
        Error::Finalizer(Box::new(e))
    })
}

fn error(_object: Arc<Whisper>, error: &Error, _ctx: Arc<Context>) -> Action {
    warn!(error = ?error, "Requeueing after error");
    // Jittered backoff (5–15s) so a batch of failing Whispers — which any tenant
    // can create — doesn't requeue in lockstep and hammer the API server every
    // second. (kube's error policy gives no per-object failure count, so this is a
    // randomized fixed backoff rather than true exponential.)
    Action::requeue(Duration::from_secs(5 + rand::random::<u64>() % 11))
}

/// Parse a comma-separated namespace list (the `WRITE_NAMESPACES` env var) into a
/// set, ignoring blanks and surrounding whitespace.
fn parse_namespaces(raw: &str) -> NSSet {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

pub async fn run(metrics: MetricState) {
    let client = Client::try_default()
        .await
        .expect("could not connect to k8s");
    let (state, lock) = start(client.clone()).await;

    // The namespaces the operator may write into (chart `writeNamespaces`). Used
    // to rediscover orphans by name-probe when status is lost (see ADR 0003); if
    // unset, recovery falls back to the status record alone.
    let write_namespaces = env::var("WRITE_NAMESPACES")
        .map(|raw| parse_namespaces(&raw))
        .unwrap_or_default();
    // Namespaces being decommissioned (chart `drainNamespaces`): probed and
    // reclaimed, never written to. Must be disjoint from write_namespaces — an
    // overlap is a contradiction (write here vs. never a target), so refuse to
    // start rather than silently let drain win.
    let drain_namespaces = env::var("DRAIN_NAMESPACES")
        .map(|raw| parse_namespaces(&raw))
        .unwrap_or_default();
    let overlap = write_namespaces
        .intersection(&drain_namespaces)
        .cloned()
        .collect::<Vec<_>>();
    if !overlap.is_empty() {
        panic!(
            "WRITE_NAMESPACES and DRAIN_NAMESPACES must be disjoint; overlapping: {}",
            overlap.join(", ")
        );
    }
    info!("write namespaces: {write_namespaces:?}; draining: {drain_namespaces:?}");

    // Source secrets live in the operator's own namespace; watch only there so we
    // never need cluster-wide secret list/watch. The root watch is on Whispers
    // (our own API group), and a changed source secret maps back to the Whisper(s)
    // that reference it via the controller's Whisper store.
    let source_ns = client.default_namespace().to_string();
    // Mark the operator's own namespace protected (fail closed, no env reliance).
    let _ = OPERATOR_NAMESPACE.set(source_ns.clone());
    let whispers = Api::<Whisper>::all(client.clone());
    let secrets = Api::<Secret>::namespaced(client.clone(), &source_ns);
    info!("watching Whispers; source secrets in namespace '{source_ns}'");
    let recorder = Recorder::new(
        client.clone(),
        Reporter {
            controller: "whisperer".into(),
            instance: env::var("CONTROLLER_POD_NAME").ok(),
        },
    );
    // Cap how many Whispers reconcile at once so a flood of tenant-created
    // Whispers can't drive unbounded concurrent API work against the cluster.
    let controller = Controller::new(whispers, Config::default())
        .with_config(ControllerConfig::default().concurrency(8));
    let store = controller.store();
    controller
        .watches(secrets, Config::default(), move |secret| {
            let name = secret.name_any();
            let ns = secret.namespace();
            store
                .state()
                .into_iter()
                .filter(|w| w.spec.secret_name == name && w.namespace() == ns)
                .map(|w| ObjectRef::from_obj(w.as_ref()))
                .collect::<Vec<_>>()
        })
        .shutdown_on_signal()
        .run(
            dispatcher,
            error,
            Arc::new(Context {
                client,
                recorder,
                metrics,
                state,
                write_namespaces,
                drain_namespaces,
            }),
        )
        .for_each(|res| async move {
            match res {
                Ok(o) => info!("reconciled {:?}", o),
                Err(RuntimeError::ObjectNotFound(_)) => info!("object already deleted"),
                Err(e) => warn!(error = ?e, "reconcile failed"),
            }
        })
        .await;
    let _ = lock.retire().await;
}

#[cfg(test)]
mod test {
    use crate::{
        election::{LeaderState, State},
        metrics::MetricState,
        whisper::{Whisper, WhisperSpec},
    };

    use super::{
        Context, NSSet, WHISPER_LABEL, apply, cleanup, is_consenting_namespace, is_owned_copy,
        is_protected_namespace, resolve_targets,
    };
    use crate::labels::{ALLOW_SYNC_LABEL, FIELD_MANAGER, OWNER_UID_LABEL};
    use k8s_openapi::{
        ByteString,
        api::core::v1::{Namespace, Secret},
        apimachinery::pkg::apis::meta::v1::ManagedFieldsEntry,
    };

    /// Build a Namespace with the given name and labels for the pure
    /// target-resolution tests.
    fn ns_with(name: &str, labels: &[(&str, &str)]) -> Namespace {
        use kube::api::ObjectMeta;
        use std::collections::BTreeMap;
        Namespace {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                labels: Some(
                    labels
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect::<BTreeMap<_, _>>(),
                ),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn wanted(names: &[&str]) -> NSSet {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn copy_with(labels: &[(&str, &str)], manager: Option<&str>) -> Secret {
        use kube::api::ObjectMeta;
        use std::collections::BTreeMap;
        Secret {
            metadata: ObjectMeta {
                labels: Some(
                    labels
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect::<BTreeMap<_, _>>(),
                ),
                managed_fields: manager.map(|m| {
                    vec![ManagedFieldsEntry {
                        manager: Some(m.to_string()),
                        ..Default::default()
                    }]
                }),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn protected_namespaces_are_rejected() {
        assert!(is_protected_namespace("kube-system"));
        assert!(is_protected_namespace("kube-public"));
        assert!(is_protected_namespace("kube-node-lease"));
        assert!(!is_protected_namespace("team-a"));
    }

    #[test]
    fn is_owned_copy_requires_attribution_and_markers() {
        let owned = |labels: &[(&str, &str)], manager| copy_with(labels, manager);
        let full = &[(WHISPER_LABEL, "true"), (OWNER_UID_LABEL, "uid-1")];

        // genuine copy owned by this Whisper: owner UID + whisper marker + our manager
        assert!(is_owned_copy(&owned(full, Some(FIELD_MANAGER)), "uid-1"));

        // attribution failures (the strong check):
        // - a copy owned by a different Whisper
        assert!(!is_owned_copy(&owned(full, Some(FIELD_MANAGER)), "uid-2"));
        // - no owner-uid label at all
        assert!(!is_owned_copy(
            &owned(&[(WHISPER_LABEL, "true")], Some(FIELD_MANAGER)),
            "uid-1"
        ));
        // - an empty owner UID must never match, even an empty label
        assert!(!is_owned_copy(&owned(full, Some(FIELD_MANAGER)), ""));
        assert!(!is_owned_copy(
            &owned(
                &[(OWNER_UID_LABEL, ""), (WHISPER_LABEL, "true")],
                Some(FIELD_MANAGER)
            ),
            ""
        ));

        // marker failures (defense-in-depth):
        // - right owner + manager but missing the whisper marker
        assert!(!is_owned_copy(
            &owned(&[(OWNER_UID_LABEL, "uid-1")], Some(FIELD_MANAGER)),
            "uid-1"
        ));
        // - right owner + marker but written by someone else's field manager
        assert!(!is_owned_copy(&owned(full, Some("attacker")), "uid-1"));
        // - nothing at all
        assert!(!is_owned_copy(&owned(&[], None), "uid-1"));
    }

    #[test]
    fn consenting_namespace_requires_allow_sync_label() {
        assert!(is_consenting_namespace(&ns_with(
            "team-a",
            &[(ALLOW_SYNC_LABEL, "true")]
        )));
        // present but not "true"
        assert!(!is_consenting_namespace(&ns_with(
            "team-a",
            &[(ALLOW_SYNC_LABEL, "false")]
        )));
        // label absent
        assert!(!is_consenting_namespace(&ns_with("team-a", &[])));
        // protected namespaces never consent, even if labelled
        assert!(!is_consenting_namespace(&ns_with(
            "kube-system",
            &[(ALLOW_SYNC_LABEL, "true")]
        )));
    }

    #[test]
    fn parse_namespaces_trims_and_drops_blanks() {
        use super::parse_namespaces;
        assert_eq!(parse_namespaces(""), NSSet::new());
        assert_eq!(
            parse_namespaces(" a , b ,, c, "),
            ["a", "b", "c"].iter().map(|s| s.to_string()).collect()
        );
    }

    #[test]
    fn resolve_targets_only_syncs_into_opted_in_namespaces() {
        let namespaces = [
            ns_with("opted-in", &[(ALLOW_SYNC_LABEL, "true")]),
            ns_with("not-opted-in", &[]),
            ns_with("kube-system", &[(ALLOW_SYNC_LABEL, "true")]),
        ];
        // Request all three plus one that doesn't exist.
        let res = resolve_targets(
            &wanted(&["opted-in", "not-opted-in", "kube-system", "ghost"]),
            &namespaces,
        );

        // Only the consenting, non-protected namespace is a target.
        assert_eq!(res.targets, wanted(&["opted-in"]));
        // An existing namespace without the opt-in label is refused (this is
        // the cross-tenant injection that gets dropped) — protected too.
        assert!(res.refused.contains(&"not-opted-in".to_string()));
        assert!(res.refused.contains(&"kube-system".to_string()));
        // A namespace that doesn't exist is unknown, not refused, not a target.
        assert_eq!(res.unknown, vec!["ghost".to_string()]);
        assert!(!res.targets.contains("ghost"));
    }

    #[test]
    fn resolve_targets_is_empty_when_nothing_opts_in() {
        let namespaces = [ns_with("a", &[]), ns_with("b", &[])];
        let res = resolve_targets(&wanted(&["a", "b"]), &namespaces);
        assert!(res.targets.is_empty());
        assert_eq!(res.refused.len(), 2);
    }

    // ---- Property tests -------------------------------------------------
    //
    // resolve_targets and plan are the pure core of the reconcile loop, so we
    // assert their invariants over arbitrary inputs rather than hand-picked
    // examples. These are the same invariants the TLA+ model proves at the
    // protocol level (see docs/model/Whisper.tla).

    use super::{SyncPlan, plan};
    use proptest::prelude::*;

    /// A namespace name drawn from a small pool so generated `wanted` and
    /// `existing` sets overlap meaningfully, and so protected names show up
    /// often enough to exercise the protected-namespace guard.
    fn ns_name() -> impl Strategy<Value = String> {
        prop_oneof![
            (0u8..6).prop_map(|n| format!("ns{n}")),
            Just("kube-system".to_string()),
            Just("kube-node-lease".to_string()),
        ]
    }

    fn ns_set() -> impl Strategy<Value = NSSet> {
        prop::collection::hash_set(ns_name(), 0..6)
    }

    proptest! {
        /// For any cluster and any request, resolve_targets sorts every
        /// requested namespace into exactly one of targets/refused/unknown,
        /// and every target is a real, consenting, non-protected namespace.
        #[test]
        fn resolve_targets_invariants(
            // existing namespaces, each either consenting or not
            existing in prop::collection::hash_map(ns_name(), any::<bool>(), 0..6),
            wanted in ns_set(),
        ) {
            let namespaces = existing
                .iter()
                .map(|(name, consents)| {
                    let labels: &[(&str, &str)] =
                        if *consents { &[(ALLOW_SYNC_LABEL, "true")] } else { &[] };
                    ns_with(name, labels)
                })
                .collect::<Vec<_>>();

            let res = resolve_targets(&wanted, &namespaces);
            let refused = res.refused.iter().cloned().collect::<NSSet>();
            let unknown = res.unknown.iter().cloned().collect::<NSSet>();

            // Subset: never a target we weren't asked for.
            prop_assert!(res.targets.is_subset(&wanted));
            // Partition: the three buckets are disjoint and cover wanted exactly.
            prop_assert!(res.targets.is_disjoint(&refused));
            prop_assert!(res.targets.is_disjoint(&unknown));
            prop_assert!(refused.is_disjoint(&unknown));
            let mut union = res.targets.clone();
            union.extend(refused.iter().cloned());
            union.extend(unknown.iter().cloned());
            prop_assert_eq!(&union, &wanted);
            // Every target is a real namespace that consents and is not protected.
            for t in &res.targets {
                prop_assert_eq!(existing.get(t), Some(&true), "target {} must consent", t);
                prop_assert!(!is_protected_namespace(t), "target {} must not be protected", t);
            }
            // unknown = exactly the requested namespaces that don't exist.
            for u in &res.unknown {
                prop_assert!(!existing.contains_key(u));
            }
        }

        /// For any prior sync state and any resolved targets, plan() produces a
        /// safe, convergent reconcile: writes and deletes never overlap, deletes
        /// are confined to namespaces we'd previously synced, and applying the
        /// plan lands the live copy set exactly on `targets`.
        #[test]
        fn plan_is_safe_and_convergent(synced in ns_set(), targets in ns_set()) {
            let SyncPlan { to_write, to_delete } = plan(&synced, &targets);

            // Safety: never delete a namespace we're about to (re)write.
            prop_assert!(to_write.is_disjoint(&to_delete));
            // We write exactly the targets...
            prop_assert_eq!(&to_write, &targets);
            // ...and only ever delete copies we previously made.
            prop_assert!(to_delete.is_subset(&synced));

            // Convergence: applying the plan to the prior copy set yields targets.
            let mut live = synced.clone();
            live.extend(to_write.iter().cloned());
            for ns in &to_delete {
                live.remove(ns);
            }
            prop_assert_eq!(&live, &targets);
        }
    }

    use kube::{
        Api, Client, ResourceExt,
        api::{DeleteParams, ListParams, ObjectMeta, Patch, PatchParams, PostParams},
        runtime::events::{Recorder, Reporter},
    };
    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_otlp::{MetricExporter, Protocol, WithExportConfig};
    use opentelemetry_sdk::metrics::SdkMeterProvider;
    use serde_json::json;
    use std::{collections::BTreeMap, env, sync::Arc};
    use tokio::sync::watch;

    #[tokio::test]
    #[ignore = "uses k8s api"]
    async fn sync_and_delete_works() {
        tracing_subscriber::fmt::init();
        let client = Client::try_default().await.unwrap();
        let nsapi: Api<Namespace> = Api::all(client.clone());
        let namespaces = ["source", "target", "target2", "clean"];
        // Targets must consent with allow-sync=true before whisperer will sync
        // into them; "source" and "clean" deliberately don't opt in.
        let consenting = ["target", "target2"];
        for ns in namespaces {
            let labels = consenting
                .contains(&ns)
                .then(|| BTreeMap::from([(ALLOW_SYNC_LABEL.to_string(), "true".to_string())]));
            nsapi
                .create(
                    &PostParams::default(),
                    &Namespace {
                        metadata: ObjectMeta {
                            name: Some(ns.to_string()),
                            labels,
                            ..ObjectMeta::default()
                        },
                        ..Namespace::default()
                    },
                )
                .await
                .unwrap();
        }

        // The source secret lives in the source namespace and carries no
        // whisperer config of its own — the Whisper resource declares the sync.
        let source_secrets: Api<Secret> = Api::namespaced(client.clone(), "source");
        let secret_patch = Secret {
            data: Some(BTreeMap::from([(
                "secret".to_string(),
                ByteString("secret".into()),
            )])),
            metadata: ObjectMeta {
                name: Some("sync".to_string()),
                namespace: Some("source".to_string()),
                ..ObjectMeta::default()
            },
            ..Secret::default()
        };
        source_secrets
            .patch(
                "sync",
                &PatchParams::apply("whisperer.jeffl.es"),
                &Patch::Apply(secret_patch.clone()),
            )
            .await
            .unwrap();

        // The Whisper declares the sync: secret `sync` in `source` → target,
        // target2, plus a non-existent namespace that should be skipped.
        let whispers: Api<Whisper> = Api::namespaced(client.clone(), "source");
        let whisper = Whisper::new(
            "sync",
            WhisperSpec {
                secret_name: "sync".to_string(),
                namespaces: vec![
                    "target".to_string(),
                    "target2".to_string(),
                    "missing".to_string(),
                ],
            },
        );
        whispers
            .patch(
                "sync",
                &PatchParams::apply(FIELD_MANAGER),
                &Patch::Apply(&whisper),
            )
            .await
            .unwrap();

        // Build a leader Context with the given write/drain namespace sets.
        let make_ctx = |write: &[&str], drain: &[&str]| {
            let recorder = Recorder::new(
                client.clone(),
                Reporter {
                    controller: "whisperer".into(),
                    instance: env::var("CONTROLLER_POD_NAME").ok(),
                },
            );
            let exporter = MetricExporter::builder()
                .with_http()
                .with_protocol(Protocol::HttpBinary)
                .build()
                .unwrap();
            let provider = SdkMeterProvider::builder()
                .with_periodic_exporter(exporter)
                .build();
            let metrics = MetricState::new(provider.meter("whisperer"));
            let (_, rx) = watch::channel(State::Leading);
            Arc::new(Context {
                client: client.clone(),
                recorder,
                metrics,
                state: LeaderState::new(rx),
                write_namespaces: write.iter().map(|s| s.to_string()).collect(),
                drain_namespaces: drain.iter().map(|s| s.to_string()).collect(),
            })
        };

        // The operator may write into both consenting targets; this is also the
        // set the orphan name-probe scans.
        let data = make_ctx(&["target", "target2"], &[]);

        // Copies are identified cluster-wide by the whisper marker label.
        let all_secrets = Api::<Secret>::all(client.clone());
        let copy_params = ListParams {
            label_selector: format!("{WHISPER_LABEL}=true").into(),
            ..Default::default()
        };

        // First reconcile: secret should land in both consenting targets.
        let whisper = Arc::new(whispers.get("sync").await.unwrap());
        let _ = apply(whisper, data.clone()).await.unwrap();
        let items = all_secrets.list(&copy_params).await.unwrap().items;
        assert_eq!(items.len(), 2, "secret is synced into both targets");

        // Status should now record both target namespaces.
        let stored = whispers.get_status("sync").await.unwrap();
        let synced = stored.status.unwrap().synced_namespaces;
        assert_eq!(synced.len(), 2, "status records both synced namespaces");

        // Simulate status loss: wipe synced_namespaces. The operator now has no
        // record of the target2 copy — only the name-probe of writeNamespaces can
        // rediscover it. This is the documented tradeoff's recovery path.
        whispers
            .patch_status(
                "sync",
                &PatchParams::default(),
                &Patch::Merge(json!({ "status": { "syncedNamespaces": [] } })),
            )
            .await
            .unwrap();

        // Narrow the spec to a single target and reconcile again. With status
        // empty, target2 is an orphan; the probe must find and reclaim it anyway,
        // leaving exactly one copy.
        let narrowed = whisper_spec_patch(vec!["target".to_string()]);
        whispers
            .patch(
                "sync",
                &PatchParams::apply(FIELD_MANAGER),
                &Patch::Apply(&narrowed),
            )
            .await
            .unwrap();
        let whisper = Arc::new(whispers.get_status("sync").await.unwrap());
        let _ = apply(whisper.clone(), data.clone()).await.unwrap();
        let items = all_secrets.list(&copy_params).await.unwrap().items;
        assert_eq!(
            items.len(),
            1,
            "orphan is reclaimed via the write-namespace probe even after status loss"
        );

        // Rename the source: point secretName at a DIFFERENT secret. Because
        // copies are named after the Whisper (not secretName), this must refresh
        // the existing "sync" copy in place — never create a new "sync2"-named
        // copy and strand the old one.
        let src2 = Secret {
            data: Some(BTreeMap::from([(
                "secret".to_string(),
                ByteString("secret2".into()),
            )])),
            metadata: ObjectMeta {
                name: Some("sync2".to_string()),
                namespace: Some("source".to_string()),
                ..ObjectMeta::default()
            },
            ..Secret::default()
        };
        source_secrets
            .patch(
                "sync2",
                &PatchParams::apply("whisperer.jeffl.es"),
                &Patch::Apply(&src2),
            )
            .await
            .unwrap();
        let renamed = Whisper::new(
            "sync",
            WhisperSpec {
                secret_name: "sync2".to_string(),
                namespaces: vec!["target".to_string()],
            },
        );
        whispers
            .patch(
                "sync",
                &PatchParams::apply(FIELD_MANAGER),
                &Patch::Apply(&renamed),
            )
            .await
            .unwrap();
        let whisper = Arc::new(whispers.get_status("sync").await.unwrap());
        let _ = apply(whisper, data.clone()).await.unwrap();
        let items = all_secrets.list(&copy_params).await.unwrap().items;
        assert_eq!(
            items.len(),
            1,
            "rename refreshes the copy in place, no orphan"
        );
        assert!(
            items.iter().all(|s| s.name_any() == "sync"),
            "copy keeps the Whisper's name across a secretName rename"
        );
        let copy = Api::<Secret>::namespaced(client.clone(), "target")
            .get("sync")
            .await
            .unwrap();
        assert_eq!(
            copy.data.unwrap().get("secret").unwrap().0,
            b"secret2",
            "copy data is refreshed from the renamed source"
        );

        // Decommission "target": move it from write to drain (it's still in the
        // spec and still consents). The operator must NOT write there anymore and
        // must reclaim the copy already in it — proving a namespace can be drained
        // without stranding copies, even before it's removed from the spec.
        let drain = make_ctx(&["target2"], &["target"]);
        let whisper = Arc::new(whispers.get_status("sync").await.unwrap());
        let _ = apply(whisper, drain).await.unwrap();
        let items = all_secrets.list(&copy_params).await.unwrap().items;
        assert_eq!(
            items.len(),
            0,
            "draining a namespace reclaims its copy instead of refreshing it"
        );

        // Cleanup reclaims any remaining copies but leaves the source secret.
        let whisper = Arc::new(whispers.get_status("sync").await.unwrap());
        let _ = cleanup(whisper, data).await.unwrap();
        let items = all_secrets.list(&copy_params).await.unwrap().items;
        assert_eq!(items.len(), 0, "all copies removed on cleanup");
        assert!(
            source_secrets.get_opt("sync").await.unwrap().is_some(),
            "source secret is left alone on cleanup"
        );

        whispers
            .delete("sync", &DeleteParams::default())
            .await
            .unwrap();
        for ns in namespaces {
            nsapi.delete(ns, &DeleteParams::default()).await.unwrap();
        }
    }

    /// Build a `Whisper` apply-patch narrowing the target namespaces.
    fn whisper_spec_patch(namespaces: Vec<String>) -> Whisper {
        Whisper::new(
            "sync",
            WhisperSpec {
                secret_name: "sync".to_string(),
                namespaces,
            },
        )
    }
}

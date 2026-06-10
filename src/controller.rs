use crate::{
    election::{LeaderState, start},
    error::{Error, Result},
    ext::SecretExt,
    labels::*,
    metrics::MetricState,
    whisper::Whisper,
};
use futures::StreamExt;
use serde_json::json;
use k8s_openapi::api::core::v1::{Namespace, ObjectReference, Secret};
use kube::{
    Api, Client, Resource, ResourceExt,
    api::{DeleteParams, ListParams, Patch, PatchParams},
    runtime::{
        Controller,
        controller::{Action, Error as RuntimeError},
        events::{Event as Notice, EventType, Recorder, Reporter},
        finalizer::{Event, finalizer},
        reflector::ObjectRef,
        watcher::Config,
    },
};
use std::{collections::HashSet, env, sync::Arc};
use tokio::time::Duration;
use tracing::{error, info, instrument, warn};

struct Context {
    client: Client,
    recorder: Recorder,
    metrics: MetricState,
    state: LeaderState,
}

impl Context {
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

/// True if `ns` must never be used as a sync target. Covers the well-known
/// system namespaces plus the operator's own namespace (set via the
/// `CONTROLLER_NAMESPACE` env var) so a tenant can't target the controller.
fn is_protected_namespace(ns: &str) -> bool {
    PROTECTED_NAMESPACES.contains(&ns)
        || env::var("CONTROLLER_NAMESPACE").ok().as_deref() == Some(ns)
}

/// True only for secrets whisperer itself created: they carry the `whisper`
/// marker label AND were last applied by our field manager. The labels alone
/// are forgeable by any tenant, but `managedFields` is maintained by the API
/// server, so requiring our manager prevents a crafted look-alike secret from
/// tricking the operator into deleting a victim's data.
fn is_managed_copy(secret: &Secret) -> bool {
    let marked = secret
        .labels()
        .get(WHISPER_LABEL)
        .map(|v| v == "true")
        .unwrap_or(false);
    let ours = secret.meta().managed_fields.as_ref().is_some_and(|fields| {
        fields
            .iter()
            .any(|f| f.manager.as_deref() == Some(FIELD_MANAGER))
    });
    marked && ours
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

/// List the cluster's namespaces and resolve `wanted` down to the eligible,
/// consenting targets, logging any that don't exist or haven't opted in.
///
/// The consent check (`whisperer.jeffl.es/allow-sync=true`) is the authorization
/// boundary: a Whisper can name any namespace, but a secret is only copied into
/// one that has explicitly opted in and isn't protected.
async fn resolve(wanted: &NSSet, client: &Client) -> Result<NSSet, Error> {
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

    Ok(resolution.targets)
}

/// Reconcile one Whisper: read its source secret, resolve the consenting
/// targets, reclaim copies in namespaces that fell out of the target set (from
/// the status record, so no cluster-wide list), write copies into the current
/// targets, and record where they now live.
#[instrument(skip(ctx, whisper))]
async fn apply(whisper: Arc<Whisper>, ctx: Arc<Context>) -> Result<Action, Error> {
    let client = &ctx.client;
    let namespace = whisper.namespace().unwrap_or_default();
    let secret_name = whisper.spec.secret_name.clone();
    info!(
        "reconciling whisper {} in {namespace} (secret {secret_name})",
        whisper.name_any()
    );

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
    let targets = resolve(&wanted, client).await?;
    let synced = whisper
        .status
        .as_ref()
        .map(|s| s.synced_namespaces.iter().cloned().collect::<NSSet>())
        .unwrap_or_default();

    // Orphans are namespaces we synced into before but aren't targets now. We
    // know them from status, so we delete by name in those specific namespaces
    // (verifying managed-copy origin) — never a cluster-wide secret list.
    for ns in synced.difference(&targets) {
        info!("reclaiming '{secret_name}' from '{ns}' (no longer a target)");
        delete_copy(secret_name.clone(), ns.clone(), ctx.clone()).await?;
        ctx.record(
            &Notice {
                type_: EventType::Normal,
                reason: "Delete Requested".into(),
                note: Some(format!("Reclaiming '{secret_name}' from '{ns}'")),
                action: "Delete".into(),
                secondary: None,
            },
            &whisper.object_ref(&()),
        )
        .await?;
    }

    // Write a copy into every current target (idempotent server-side apply).
    for ns in &targets {
        let api: Api<Secret> = Api::namespaced(client.clone(), ns);
        let copy = secret.dup(ns.clone());
        let res = api
            .patch(
                &secret_name,
                &PatchParams::apply(FIELD_MANAGER),
                &Patch::Apply(&copy),
            )
            .await
            .map_err(Error::Patch)?;
        ctx.record(
            &Notice {
                type_: EventType::Normal,
                reason: "Sync Requested".into(),
                note: Some(format!("Synced '{secret_name}' from '{namespace}' to '{ns}'")),
                action: "Sync".into(),
                secondary: Some(res.object_ref(&())),
            },
            &whisper.object_ref(&()),
        )
        .await?;
        info!("whispered '{secret_name}' from '{namespace}' to '{ns}'");
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

async fn delete(name: String, namespace: String, ctx: Arc<Context>) -> Result<()> {
    let api: Api<Secret> = Api::namespaced(ctx.client.clone(), &namespace);
    api.delete(&name, &DeleteParams::default())
        .await
        .map_err(Error::Delete)?
        .map_left(|_| info!("deleting secret {name} in {namespace}"))
        .map_right(|_| info!("deleted secret {name} in {namespace}"));
    Ok(())
}

/// Delete a secret only if it is genuinely a whisperer-managed copy.
///
/// Used for every cross-namespace delete: we fetch the target and confirm it
/// passes [`is_managed_copy`] before removing it, so a tenant who plants a
/// look-alike secret (right name/labels, wrong origin) can't trick the operator
/// into destroying their data. A missing secret is a no-op.
async fn delete_copy(name: String, namespace: String, ctx: Arc<Context>) -> Result<()> {
    let api: Api<Secret> = Api::namespaced(ctx.client.clone(), &namespace);
    match api.get_opt(&name).await.map_err(Error::GetSecret)? {
        Some(secret) if is_managed_copy(&secret) => delete(name, namespace, ctx).await,
        Some(_) => {
            warn!("refusing to delete secret {name} in {namespace}: not a whisperer-managed copy");
            Ok(())
        }
        None => Ok(()),
    }
}

/// Does three things:
/// 1. Delete the `secret.`
/// 2. Delete child secrets in namespaces in the secret's annotation.
/// 3. Delete secrets that reference the `secret` but aren't in the annotation's namespace list, just in case.
#[instrument(skip(ctx, secret))]
async fn cleanup(secret: Arc<Secret>, ctx: Arc<Context>) -> Result<Action> {
    let name = secret.name_any();
    let namespace = secret.namespace().unwrap_or(String::from(""));
    delete(name.clone(), namespace.clone(), ctx.clone()).await?;
    // First we delete the secrets in the namespaces listed on the secret.
    let union = secret_namespaces(secret.clone(), ctx.client.clone()).await?;
    let namespaces = union
        .clone()
        .into_iter()
        .collect::<Vec<String>>()
        .join(", ");
    ctx.record(
        &Notice {
            type_: EventType::Normal,
            reason: "Delete Requested".into(),
            note: Some(format!(
                "Deleting {name} in {namespace} and namespaces {namespaces}"
            )),
            action: "Delete".into(),
            secondary: None,
        },
        &secret.object_ref(&()),
    )
    .await?;
    for ns in union.clone() {
        delete_copy(name.clone(), ns, ctx.clone()).await?;
    }

    // And just to be absolutely sure we delete any remaining secrets that have our child labels on them,
    // but aren't in the secrets namespace list. This may only happen if there's a race where a
    // synced secret's namespaces have changed and it's then immediately deleted (even then it would
    // be real tricky to get right), but it's worth being careful here.
    let api: Api<Secret> = Api::<Secret>::all(ctx.client.clone());
    let secrets = api
        .list(&ListParams {
            label_selector: Some(format!(
                "{WHISPER_LABEL}=true,{NAME_LABEL}={},{NAMESPACE_LABEL}={}",
                name.clone(),
                namespace.clone()
            )),
            ..Default::default()
        })
        .await
        .map_err(Error::ListSecrets)?;
    for secret in secrets {
        let namespace = secret.namespace().unwrap_or(String::from(""));
        // We hold the object here, so verify origin directly before deleting.
        if !union.contains(&namespace) && is_managed_copy(&secret) {
            delete(secret.name_any(), namespace, ctx.clone()).await?;
        }
    }
    Ok(Action::await_change())
}

#[instrument(skip(ctx))]
async fn dispatcher(secret: Arc<Secret>, ctx: Arc<Context>) -> Result<Action> {
    if !ctx.state.is_leader() {
        info!(
            "not leader (leader is {}), ignoring change",
            ctx.state.leader()
        );
        return Ok(Action::await_change());
    }
    let metrics = ctx.metrics.clone();
    let namespace = secret.namespace().unwrap_or(String::from(""));
    // Hold the duration guard for the whole reconcile; it records on drop.
    let _record = metrics.duration();
    let api: Api<Secret> = Api::namespaced(ctx.client.clone(), &namespace);
    finalizer(&api, FINALIZER, secret, |event| async {
        metrics.reconcile();
        match event {
            Event::Apply(secret) => apply(secret, ctx).await,
            Event::Cleanup(secret) => cleanup(secret, ctx).await,
        }
    })
    .await
    .map_err(|e| {
        metrics.failure();
        Error::Finalizer(Box::new(e))
    })
}

fn error(_object: Arc<Secret>, error: &Error, _ctx: Arc<Context>) -> Action {
    warn!(error = ?error, "Requeueing after error");
    Action::requeue(Duration::from_secs(1))
}

pub async fn run(metrics: MetricState) {
    let client = Client::try_default()
        .await
        .expect("could not connect to k8s");

    let (state, lock) = start(client.clone()).await;
    let root = Config::default().labels(&format!("{ACTIVE_LABEL}=true"));
    let related = Config::default().labels(&format!("{WHISPER_LABEL}=true"));
    let api = Api::<Secret>::all(client.clone());
    info!("watching secrets with label {ACTIVE_LABEL}");
    let recorder = Recorder::new(
        client.clone(),
        Reporter {
            controller: "whisperer".into(),
            instance: env::var("CONTROLLER_POD_NAME").ok(),
        },
    );
    Controller::new(api.clone(), root)
        .watches(api, related, |secret| {
            let ns = secret.labels().get(NAMESPACE_LABEL)?;
            Some(ObjectRef::new(&secret.name_any()).within(ns))
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
    };

    use super::{
        ACTIVE_LABEL, Context, NAMESPACE_ANNOTATION, NSSet, WHISPER_LABEL, apply, cleanup,
        is_consenting_namespace, is_managed_copy, is_protected_namespace, resolve_targets,
    };
    use crate::labels::{ALLOW_SYNC_LABEL, FIELD_MANAGER, NAME_LABEL, NAMESPACE_LABEL};
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
    fn managed_copy_requires_both_marker_and_field_manager() {
        // genuine copy: whisper=true AND our field manager
        assert!(is_managed_copy(&copy_with(
            &[
                (WHISPER_LABEL, "true"),
                (NAME_LABEL, "db"),
                (NAMESPACE_LABEL, "src")
            ],
            Some(FIELD_MANAGER),
        )));
        // forged labels but written by someone else -> not ours, must not delete
        assert!(!is_managed_copy(&copy_with(
            &[
                (WHISPER_LABEL, "true"),
                (NAME_LABEL, "db"),
                (NAMESPACE_LABEL, "src")
            ],
            Some("attacker"),
        )));
        // our manager but no whisper marker -> not a managed copy
        assert!(!is_managed_copy(&copy_with(
            &[(NAME_LABEL, "db")],
            Some(FIELD_MANAGER),
        )));
        // nothing -> not a managed copy
        assert!(!is_managed_copy(&copy_with(&[], None)));
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
    use kube::{
        Api, Client,
        api::{DeleteParams, ListParams, ObjectMeta, Patch, PatchParams, PostParams},
        runtime::events::{Recorder, Reporter},
    };
    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_otlp::{MetricExporter, Protocol, WithExportConfig};
    use opentelemetry_sdk::metrics::SdkMeterProvider;
    use std::{collections::BTreeMap, env, sync::Arc};
    use tokio::sync::watch;

    #[tokio::test]
    #[ignore = "uses k8s api"]
    async fn sync_and_delete_works() {
        tracing_subscriber::fmt::init();
        let client = Client::try_default().await.unwrap();
        let lp = ListParams::default().labels(&format!("{ACTIVE_LABEL}=true"));
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
        let api: Api<Secret> = Api::namespaced(client.clone(), "source");
        let secret_patch = Secret {
            data: Some(BTreeMap::from([(
                "secret".to_string(),
                ByteString("secret".into()),
            )])),
            metadata: ObjectMeta {
                annotations: Some(BTreeMap::from([(
                    NAMESPACE_ANNOTATION.to_string(),
                    "target,target2,missing".to_string(),
                )])),
                labels: Some(BTreeMap::from([(
                    ACTIVE_LABEL.to_string(),
                    "true".to_string(),
                )])),
                name: Some("sync".to_string()),
                namespace: Some("source".to_string()),
                ..ObjectMeta::default()
            },
            ..Secret::default()
        };
        let _ = api
            .patch(
                "sync",
                &PatchParams::apply("whisperer.jeffl.es"),
                &Patch::Apply(secret_patch.clone()),
            )
            .await
            .unwrap();

        let api = Api::<Secret>::all(client.clone());
        let items = api.list(&lp).await.unwrap().items;
        assert!(items.len() == 1, "secret created");

        // TODO: make this a method
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
        let meter = provider.meter("whisperer");

        let metrics = MetricState::new(meter);
        let (_, rx) = watch::channel(State::Leading);
        let data = Arc::new(Context {
            client: client.clone(),
            recorder,
            metrics,
            state: LeaderState::new(rx),
        });

        let secret = Arc::new(items.first().unwrap().to_owned());
        let _ = apply(secret.clone(), data.clone()).await.unwrap();
        let whisper_params = ListParams {
            label_selector: format!("{WHISPER_LABEL}=true").into(),
            ..Default::default()
        };
        let items = api.list(&whisper_params).await.unwrap().items;
        assert_eq!(items.len(), 2, "secret is synced");

        let patch = Api::<Secret>::namespaced(client, "source");
        let mut secret_patch = secret_patch.clone();
        secret_patch.metadata.annotations = Some(BTreeMap::from([(
            NAMESPACE_ANNOTATION.to_string(),
            "target".to_string(),
        )]));
        let _ = patch
            .patch(
                "sync",
                &PatchParams::apply("whisperer.jeffl.es"),
                &Patch::Apply(secret_patch),
            )
            .await
            .unwrap();
        let secret = Arc::new(patch.get("sync").await.unwrap());
        let _ = apply(secret.clone(), data.clone()).await.unwrap();
        let items = api.list(&whisper_params).await.unwrap().items;
        assert_eq!(
            items.len(),
            1,
            "child secret is removed when namespace is removed"
        );

        let _ = cleanup(secret, data).await.unwrap();
        let items = api.list(&lp).await.unwrap().items;
        assert_eq!(items.len(), 0, "secret is removed");
        let items = api.list(&whisper_params).await.unwrap().items;
        assert_eq!(
            items.len(),
            0,
            "child secrets is removed when parent is removed"
        );

        for ns in namespaces {
            nsapi.delete(ns, &DeleteParams::default()).await.unwrap();
        }
    }
}

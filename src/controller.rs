use crate::{
    error::{Error, Result},
    ext::SecretExt,
    labels::*,
    metrics::MetricState,
};
use futures::StreamExt;
use k8s_openapi::api::core::v1::{Namespace, ObjectReference, Secret};
use kube::{
    api::{DeleteParams, ListParams, Patch, PatchParams},
    runtime::{
        controller::{Action, Error as RuntimeError},
        events::{Event as Notice, EventType, Recorder, Reporter},
        finalizer::{finalizer, Event},
        reflector::ObjectRef,
        watcher::Config,
        Controller,
    },
    Api, Client, Resource, ResourceExt,
};
use std::{collections::HashSet, env, sync::Arc};
use tokio::time::Duration;
use tracing::{error, info, instrument, warn};

struct Context {
    client: Client,
    recorder: Recorder,
    metrics: MetricState,
}

impl Context {
    async fn record(&self, notice: &Notice, ref_: &ObjectReference) -> Result<()> {
        self.recorder
            .publish(notice, ref_)
            .await
            .map_err(Error::Event)?;
        Ok(())
    }
}

type NSSet = HashSet<String>;

/// Returns a list of existing namespaces from a secret's namespace annotation:
/// whisperer.jeffl.es/namespaces. It is an error to sync a secret without the namespace
/// annotation. Unknown namespaces are ignored.
async fn secret_namespaces(secret: Arc<Secret>, client: Client) -> Result<NSSet, Error> {
    let namespaces = Api::<Namespace>::all(client.clone())
        .list(&ListParams::default())
        .await
        .map_err(Error::ListNamespaces)?
        .iter()
        .map(|ns| ns.name_any())
        .collect::<NSSet>();
    let annotations = secret.annotations();
    let wanted = if let Some(ns) = annotations.get(NAMESPACE_ANNOTATION) {
        ns.split(",").map(String::from).collect::<NSSet>()
    } else {
        let name = secret.name_any();
        let namespace = secret.namespace().unwrap_or(String::from(""));
        error!("missing namespace annotation {NAMESPACE_ANNOTATION} on secret '{name}' in namespace '{namespace}', not syncing");
        return Err(Error::MissingDestinationAnnotation { name, namespace });
    };
    let difference = wanted
        .difference(&namespaces)
        .cloned()
        .collect::<Vec<String>>();
    if !difference.is_empty() {
        let unk = difference.join(",");
        let name = secret.name_any();
        let namespace = secret.namespace().unwrap_or(String::from(""));
        warn!("List label {NAMESPACE_ANNOTATION} on secret '{name}' in namespace '{namespace}' includes unknown namespaces '{unk}'");
    }
    Ok(namespaces
        .intersection(&wanted)
        .map(|k| k.to_owned())
        .collect::<NSSet>())
}

/// The main reconcile function. The algorithm here is simple:
/// 1. If the secret's list of target namespaces does not include a namespace but there are secrets
///    in that namespace with child labels, delete those secrets.
/// 2. Loop through the namespaces listed in the secret's target annotation and whisper secrets
///    from the provided parent secret.
#[instrument(skip(ctx))]
async fn apply(secret: Arc<Secret>, ctx: Arc<Context>) -> Result<Action, Error> {
    let labels = secret.labels();
    let name = secret.name_any();
    let namespace = secret.namespace().unwrap_or(String::from(""));
    // test invariant: we don't have a whisperer.jeffl.es/sync label, strange! I don't think
    // this should happen. But just in case we catch it.
    if !labels.contains_key(ACTIVE_LABEL) {
        return Err(Error::MissingLabel { name, namespace });
    }

    info!("reconciling {name} in {namespace}");
    let client = &ctx.client;
    let intersection = secret_namespaces(secret.clone(), client.clone()).await?;

    // If our list of namespaces has changed remove the old orphaned secrets.
    let lp = ListParams {
        label_selector: Some(format!("{NAMESPACE_LABEL}={namespace},{NAME_LABEL}={name}")),
        ..Default::default()
    };
    let api = Api::<Secret>::all(client.clone());
    let orphans = api.list(&lp).await.map_err(Error::ListSecrets)?;
    // An orphan is a Secret that matches our source name, but isn't in our namespace list.
    // We delete it here.
    let orphans = orphans.iter().filter(|it| {
        it.name_any() == name && !intersection.contains(&it.namespace().unwrap_or(String::from("")))
    });
    for orphan in orphans {
        let ns = orphan.namespace().unwrap_or(String::from(""));
        info!(
            "namespace {} is not in {NAMESPACE_LABEL} on {name}, deleting child secret",
            ns.clone()
        );
        delete(
            orphan.name_any(),
            // this shouldn't be blank, but making the compiler happy.
            namespace.clone(),
            ctx.clone(),
        )
        .await?;
        ctx.record(
            &Notice {
                type_: EventType::Normal,
                reason: "Delete Requested".into(),
                note: Some(format!(
                    "Deleting {name} in {ns} that's not in the {NAMESPACE_LABEL} in {namespace}"
                )),
                action: "Delete".into(),
                secondary: None,
            },
            &secret.object_ref(&()),
        )
        .await?;
    }

    // Patch and create new objects, this might happen multiple times, but we don't care because it
    // is idempotent
    for ns in intersection {
        let api: Api<Secret> = Api::namespaced(client.clone(), &ns.clone());
        let secret = (*secret).clone().dup(ns.clone());
        let patch = Patch::Apply(&secret);
        let res = api
            .patch(&name, &PatchParams::apply("whisperer.jeffl.es"), &patch)
            .await
            .map_err(Error::Patch)?;
        ctx.record(
            &Notice {
                type_: EventType::Normal,
                reason: "Sync Requested".into(),
                note: Some(format!("Synced {name} from {namespace} to {ns}")),
                action: "Sync".into(),
                secondary: Some(res.object_ref(&())),
            },
            &secret.object_ref(&()),
        )
        .await?;
        info!("created whisper of {name} from {namespace} to {ns}");
    }

    Ok(Action::requeue(Duration::from_secs(300)))
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
/// Does three things:
/// 1. Delete the `secret.`
/// 2. Delete child secrets in namespaces in the secret's annotation.
/// 3. Delete secrets that reference the `secret` but aren't in the annotation's namespace list, just in case.
#[instrument(skip(ctx))]
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
        delete(name.clone(), ns, ctx.clone()).await?;
    }

    // And just to be absolutely sure we delete any remaining secreats that have our child labels on them,
    // but aren't in the secrets namespace list. This may only happen if there's a race where a
    // synced secret namespaces have changed and it's then immediately deleted (even then it would
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
        if !union.contains(&namespace) {
            delete(secret.name_any(), namespace, ctx.clone()).await?;
        }
    }
    Ok(Action::await_change())
}

#[instrument(skip(ctx))]
async fn dispatcher(secret: Arc<Secret>, ctx: Arc<Context>) -> Result<Action> {
    let metrics = ctx.metrics.clone();
    let namespace = secret.namespace().unwrap_or(String::from(""));
    let name = secret.name_any();
    // NEAT! It's very cool that this sticks around across threads and await, rust is excellent
    let _ = metrics.duration(namespace.clone(), name.clone());
    let api: Api<Secret> = Api::namespaced(ctx.client.clone(), &namespace);
    finalizer(&api, FINALIZER, secret, |event| async {
        metrics.reconcile(namespace.clone(), name.clone());
        match event {
            Event::Apply(secret) => apply(secret, ctx).await,
            Event::Cleanup(secret) => cleanup(secret, ctx).await,
        }
    })
    .await
    .map_err(|e| {
        metrics.failure(namespace.clone(), name);
        Error::Finalizer(Box::new(e))
    })
}

fn error(_object: Arc<Secret>, error: &Error, _ctx: Arc<Context>) -> Action {
    warn!("Requeueing after error {error}");
    Action::requeue(Duration::from_secs(1))
}

pub async fn run(metrics: MetricState) {
    let client = Client::try_default()
        .await
        .expect("could not connect to k8s");
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
            }),
        )
        .for_each(|res| async move {
            match res {
                Ok(o) => info!("reconciled {:?}", o),
                Err(RuntimeError::ObjectNotFound(_)) => info!("object already deleted"),
                Err(e) => warn!("reconcile failed: {}", e),
            }
        })
        .await;
}

#[cfg(test)]
mod test {
    use crate::metrics::MetricState;

    use super::{apply, cleanup, Context, ACTIVE_LABEL, NAMESPACE_ANNOTATION, WHISPER_LABEL};
    use k8s_openapi::{
        api::core::v1::{Namespace, Secret},
        ByteString,
    };
    use kube::{
        api::{DeleteParams, ListParams, ObjectMeta, Patch, PatchParams, PostParams},
        runtime::events::{Recorder, Reporter},
        Api, Client,
    };
    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_sdk::metrics::SdkMeterProvider;
    use prometheus::Registry;
    use std::{collections::BTreeMap, env, sync::Arc};

    #[tokio::test]
    #[ignore = "uses k8s api"]
    async fn sync_and_delete_works() {
        tracing_subscriber::fmt::init();
        let client = Client::try_default().await.unwrap();
        let lp = ListParams::default().labels(&format!("{ACTIVE_LABEL}=true"));
        let nsapi: Api<Namespace> = Api::all(client.clone());
        let namespaces = ["source", "target", "target2", "clean"];
        for ns in namespaces {
            nsapi
                .create(
                    &PostParams::default(),
                    &Namespace {
                        metadata: ObjectMeta {
                            name: Some(ns.to_string()),
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
        let recorder = Recorder::new(
            client.clone(),
            Reporter {
                controller: "whisperer".into(),
                instance: env::var("CONTROLLER_POD_NAME").ok(),
            },
        );

        // TODO: make this a method
        let registry = Registry::new();
        let exporter = opentelemetry_prometheus::exporter()
            .with_registry(registry.clone())
            .build()
            .unwrap();
        let provider = SdkMeterProvider::builder().with_reader(exporter).build();
        let meter = provider.meter("whisperer");

        let metrics = MetricState::new(registry, meter);
        let data = Arc::new(Context {
            client: client.clone(),
            recorder,
            metrics,
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

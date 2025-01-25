use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use futures::StreamExt;
use k8s_openapi::api::core::v1::{Namespace, Secret};
use kube::api::{DeleteParams, ListParams, ObjectMeta, Patch, PatchParams};
use kube::runtime::controller::{Action, Error as RuntimeError};
use kube::runtime::finalizer::Event;
use kube::runtime::reflector::ObjectRef;
use kube::runtime::watcher::Config;

use kube::runtime::{finalizer, Controller};
use kube::{Api, Client, ResourceExt};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::SpanExporter;
use opentelemetry_sdk::runtime;
use opentelemetry_sdk::trace::{RandomIdGenerator, TracerProvider};
use tokio::time::Duration;
use tracing::{error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

const ACTIVE_LABEL: &str = "whisperer.jeffl.es/sync";
const NAMESPACE_ANNOTATION: &str = "whisperer.jeffl.es/namespaces";
const WHISPER_LABEL: &str = "whisperer.jeffl.es/whisper";
const NAME_LABEL: &str = "whisperer.jeffl.es/name";
const NAMESPACE_LABEL: &str = "whisperer.jeffl.es/namespace";
const FINALIZER: &str = "whisperer.jeffl.es/cleanup";

use thiserror::Error;
use whisper::server::serve;
#[derive(Error, Debug)]
pub enum Error {
    #[error("Could not retrieve namespaces: {0}")]
    ListNamespaces(#[source] kube::Error),
    #[error("Could not retrieve secrets: {0}")]
    ListSecrets(#[source] kube::Error),
    #[error("Could not find label {ACTIVE_LABEL} for secret {name} in namespace {namespace}")]
    MissingLabel { name: String, namespace: String },
    #[error("Could not find destination annotation for secret {name} in namespace {namespace}")]
    MissingDestinationAnnotation { name: String, namespace: String },
    #[error("Could not patch secret: {0}")]
    Patch(#[source] kube::Error),
    #[error("Could not delete secret: {0}")]
    Delete(#[source] kube::Error),
    #[error("Could not apply finalizers: {0}")]
    Finalizer(#[source] Box<dyn std::error::Error + Send>),
}
type Result<T, E = Error> = std::result::Result<T, E>;

struct Data {
    client: Client,
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
async fn apply(secret: Arc<Secret>, ctx: Arc<Data>) -> Result<Action, Error> {
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
        let namespace = orphan.namespace().unwrap_or(String::from(""));
        info!("namespace {namespace} is not in {NAMESPACE_LABEL} on {name}, deleting child secret");
        delete(
            orphan.name_any(),
            // this shouldn't be blank, but making the compiler happy.
            namespace,
            ctx.clone(),
        )
        .await?;
    }

    // Patch and create new objects, this might happen multiple times, but we don't care because it
    // is idempotent
    for ns in intersection {
        let api: Api<Secret> = Api::namespaced(client.clone(), &ns);
        let secret = (*secret).clone();
        let meta = secret.metadata.clone();
        // I could an argument to see not syncing labels and annotations.
        let mut labels = labels
            .clone()
            .iter()
            .filter(|it| it.0 != ACTIVE_LABEL)
            .map(|it| (it.0.to_owned(), it.1.to_owned()))
            .collect::<BTreeMap<String, String>>();
        labels.insert(NAMESPACE_LABEL.to_string(), namespace.clone());
        labels.insert(NAME_LABEL.to_string(), name.clone());
        labels.insert(WHISPER_LABEL.to_string(), "true".to_string());
        let annotations = secret
            .annotations()
            .clone()
            .iter()
            .filter(|it| it.0 != NAMESPACE_ANNOTATION)
            .map(|it| (it.0.to_owned(), it.1.to_owned()))
            .collect::<BTreeMap<String, String>>();
        let finalizers = meta
            .finalizers
            .map(|it| it.into_iter().filter(|it| it != FINALIZER).collect());
        let dest = Secret {
            data: secret.data,
            immutable: secret.immutable,
            metadata: ObjectMeta {
                annotations: Some(annotations),
                deletion_grace_period_seconds: meta.deletion_grace_period_seconds,
                finalizers,
                generate_name: meta.generate_name,
                labels: Some(labels),
                name: meta.name,
                namespace: Some(ns.clone()),
                self_link: meta.self_link,
                ..ObjectMeta::default()
            },
            string_data: secret.string_data,
            type_: secret.type_,
        };
        let patch = Patch::Apply(&dest);
        api.patch(&name, &PatchParams::apply("whisperer.jeffl.es"), &patch)
            .await
            .map_err(Error::Patch)?;
        info!("created whisper of {name} from {namespace} to {ns}");
    }

    Ok(Action::requeue(Duration::from_secs(300)))
}

async fn delete(name: String, namespace: String, ctx: Arc<Data>) -> Result<()> {
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
async fn cleanup(secret: Arc<Secret>, ctx: Arc<Data>) -> Result<Action> {
    let name = secret.name_any();
    let namespace = secret.namespace().unwrap_or(String::from(""));
    delete(name.clone(), namespace.clone(), ctx.clone()).await?;
    // First we delete the secrets in the namespaces listed on the secret.
    let union = secret_namespaces(secret, ctx.client.clone()).await?;
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

async fn dispatcher(secret: Arc<Secret>, ctx: Arc<Data>) -> Result<Action> {
    let api: Api<Secret> = Api::namespaced(
        ctx.client.clone(),
        &secret.namespace().unwrap_or(String::from("")),
    );
    finalizer(&api, FINALIZER, secret, |event| async {
        match event {
            Event::Apply(secret) => apply(secret, ctx).await,
            Event::Cleanup(secret) => cleanup(secret, ctx).await,
        }
    })
    .await
    .map_err(|e| Error::Finalizer(Box::new(e)))
}

fn error(_object: Arc<Secret>, error: &Error, _ctx: Arc<Data>) -> Action {
    warn!("Requeueing after error {error}");
    Action::requeue(Duration::from_secs(1))
}

async fn health() -> &'static str {
    "feel my heartbeat moving to the beat"
}

// TODO: move to controller
async fn run() {
    let client = Client::try_default()
        .await
        .expect("could not connect to k8s");
    let root = Config::default().labels(&format!("{ACTIVE_LABEL}=true"));
    let related = Config::default().labels(&format!("{WHISPER_LABEL}=true"));
    let api = Api::<Secret>::all(client.clone());
    info!("watching secrets with label {ACTIVE_LABEL}");
    Controller::new(api.clone(), root)
        .watches(api, related, |secret| {
            let ns = secret.labels().get(NAMESPACE_LABEL)?;
            Some(ObjectRef::new(&secret.name_any()).within(ns))
        })
        .shutdown_on_signal()
        .run(dispatcher, error, Arc::new(Data { client }))
        .for_each(|res| async move {
            match res {
                Ok(o) => info!("reconciled {:?}", o),
                Err(RuntimeError::ObjectNotFound(_)) => info!("object already deleted"),
                Err(e) => warn!("reconcile failed: {}", e),
            }
        })
        .await;
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let exporter = SpanExporter::builder().with_tonic().build().unwrap();
    let tracer = TracerProvider::builder()
        .with_id_generator(RandomIdGenerator::default())
        .with_batch_exporter(exporter, runtime::Tokio)
        .build();
    let otel = tracing_opentelemetry::layer().with_tracer(tracer.tracer("whisperer"));
    let filter = EnvFilter::from_default_env();
    tracing_subscriber::registry()
        .with(otel)
        .with(fmt::layer().with_filter(filter))
        .init();
    let controller = run();
    let server = serve();
    tokio::join!(controller, server).1?;
    Ok(())
}

#[cfg(test)]
mod test {
    use super::{apply, cleanup, Data, ACTIVE_LABEL, NAMESPACE_ANNOTATION, WHISPER_LABEL};
    use k8s_openapi::{
        api::core::v1::{Namespace, Secret},
        ByteString,
    };
    use kube::{
        api::{DeleteParams, ListParams, ObjectMeta, Patch, PatchParams, PostParams},
        Api, Client,
    };
    use std::{collections::BTreeMap, sync::Arc};

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
        let data = Arc::new(Data {
            client: client.clone(),
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

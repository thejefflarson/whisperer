use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use futures::StreamExt;
use k8s_openapi::api::core::v1::{Namespace, Secret};
use kube::api::{DeleteParams, ListParams, ObjectMeta, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::finalizer::Event;
use kube::runtime::watcher::Config;

use kube::runtime::{finalizer, Controller};
use kube::{Api, Client, ResourceExt};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::SpanExporter;
use opentelemetry_sdk::trace::TracerProvider;
use tokio::time::Duration;
use tracing::{info, warn};
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

const ACTIVE_LABEL: &str = "secret-syncer.jeffl.es/sync";
const NAMESPACE_ANNOTATION: &str = "secret-syncer.jeffl.es/namespaces";
const MIRRORED_LABEL: &str = "secret-syncer.jeffl.es/mirror";
const FINALIZER: &str = "secret-syncer.jeffl.es/cleanup";

use thiserror::Error;
#[derive(Error, Debug)]
pub enum Error {
    #[error("Could not retrieve namespaces: {0}")]
    GetNamespacesFailed(#[source] kube::Error),
    #[error("Could not find label {ACTIVE_LABEL} for secret {name} in namespace {namespace}")]
    MissingLabel { name: String, namespace: String },
    #[error("Could not find destination annotation for secret {name} in namespace {namespace}")]
    MissingDestinationAnnotation { name: String, namespace: String },
    #[error("Could not patch secret: {0}")]
    PatchError(#[source] kube::Error),
    #[error("Could not delete secret: {0}")]
    DeleteError(#[source] kube::Error),
    #[error("Could not apply finalizers: {0}")]
    FinalizerError(#[source] Box<dyn std::error::Error + Send>),
}
type Result<T, E = Error> = std::result::Result<T, E>;

struct Data {
    client: Client,
}

type NSSet = HashSet<String>;

async fn secret_namespaces(secret: Arc<Secret>, client: Client) -> Result<NSSet, Error> {
    let namespaces = Api::<Namespace>::all(client.clone())
        .list(&ListParams::default())
        .await
        .map_err(Error::GetNamespacesFailed)?
        .iter()
        .map(|ns| ns.name_any())
        .collect::<NSSet>();
    let annotations = secret.annotations();
    let wanted = if let Some(ns) = annotations.get(NAMESPACE_ANNOTATION) {
        ns.split(",").map(String::from).collect::<NSSet>()
    } else {
        let name = secret.name_any();
        let namespace = secret.namespace().unwrap_or(String::from(""));
        warn!("missing namespace annotation {NAMESPACE_ANNOTATION} on secret '{name}' in namespace '{namespace}', not syncing");
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

async fn apply(secret: Arc<Secret>, ctx: Arc<Data>) -> Result<Action, Error> {
    let labels = secret.labels();
    let name = secret.name_any();
    let namespace = secret.namespace().unwrap_or(String::from(""));
    // test invariant: that we don't have an ACTIVE_KEY label, I don't think this should happen
    if !labels.contains_key(ACTIVE_LABEL) {
        return Err(Error::MissingLabel { name, namespace });
    }
    let client = &ctx.client;
    let intersection = secret_namespaces(secret.clone(), client.clone()).await?;
    for ns in intersection {
        let api: Api<Secret> = Api::namespaced(client.clone(), &ns);
        let secret = (*secret).clone();
        let meta = secret.metadata.clone();
        let mut labels = labels
            .clone()
            .iter()
            .filter(|it| it.0 != ACTIVE_LABEL)
            .map(|it| (it.0.to_owned(), it.1.to_owned()))
            .collect::<BTreeMap<String, String>>();
        labels.insert(MIRRORED_LABEL.to_string(), "true".to_string());
        let annotations = secret
            .annotations()
            .clone()
            .iter()
            .filter(|it| it.0 != NAMESPACE_ANNOTATION)
            .map(|it| (it.0.to_owned(), it.1.to_owned()))
            .collect::<BTreeMap<String, String>>();
        let dest = Secret {
            data: secret.data,
            immutable: secret.immutable,
            metadata: ObjectMeta {
                annotations: Some(annotations),
                deletion_grace_period_seconds: meta.deletion_grace_period_seconds,
                finalizers: meta.finalizers,
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
        api.patch(&name, &PatchParams::apply("secret-syncer.jeffl.es"), &patch)
            .await
            .map_err(Error::PatchError)?;
        info!("created mirror of {name} from {namespace} to {ns}");
    }
    Ok(Action::requeue(Duration::from_secs(300)))
}

async fn delete(name: String, namespace: String, ctx: Arc<Data>) -> Result<()> {
    let api: Api<Secret> = Api::namespaced(ctx.client.clone(), &namespace);
    api.delete(&name, &DeleteParams::default())
        .await
        .map_err(Error::DeleteError)?
        .map_left(|_| info!("deleting secret {name} in {namespace}"))
        .map_right(|_| info!("deleted secret {name} in {namespace}"));
    Ok(())
}

async fn cleanup(secret: Arc<Secret>, ctx: Arc<Data>) -> Result<Action> {
    let name = secret.name_any();
    let namespace = secret.namespace().unwrap_or(String::from(""));
    let union = secret_namespaces(secret, ctx.client.clone()).await?;
    for ns in union {
        delete(name.clone(), ns, ctx.clone()).await?;
    }
    delete(name, namespace, ctx.clone()).await?;
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
    .map_err(|e| Error::FinalizerError(Box::new(e)))
}

fn error(_object: Arc<Secret>, error: &Error, _ctx: Arc<Data>) -> Action {
    warn!("Requeueing after error {error}");
    Action::requeue(Duration::from_secs(1))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let exporter = SpanExporter::builder().with_tonic().build().unwrap();
    let tracer = TracerProvider::builder()
        .with_simple_exporter(exporter)
        .build();
    opentelemetry::global::set_tracer_provider(tracer.clone());
    let otel = OpenTelemetryLayer::new(tracer.tracer("secret-syncer"));
    let filter = EnvFilter::from_default_env();
    tracing_subscriber::registry()
        .with(otel)
        .with(fmt::layer().with_filter(filter))
        .init();
    let client = Client::try_default()
        .await
        .expect("could not connect to k8s");
    let watcher = Config::default().labels(&format!("{ACTIVE_LABEL}=true"));
    let api = Api::<Secret>::all(client.clone());
    info!("watching secrets with label {ACTIVE_LABEL}");
    Controller::new(api, watcher)
        .run(dispatcher, error, Arc::new(Data { client }))
        .for_each(|res| async move {
            match res {
                Ok(o) => info!("reconciled {:?}", o),
                Err(e) => warn!("reconcile failed: {}", e),
            }
        })
        .await;
    Ok(())
}

#[cfg(test)]
mod test {
    use std::{collections::BTreeMap, sync::Arc};

    use crate::NAMESPACE_ANNOTATION;

    use super::{apply, cleanup, Data, ACTIVE_LABEL};
    use k8s_openapi::{
        api::core::v1::{Namespace, Secret},
        ByteString,
    };
    use kube::{
        api::{DeleteParams, ListParams, ObjectMeta, PostParams},
        Api, Client,
    };

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
        api.create(
            &PostParams::default(),
            &Secret {
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
            },
        )
        .await
        .unwrap();
        let items = api.list(&lp).await.unwrap().items;
        assert!(items.len() == 1);
        let data = Arc::new(Data { client });
        let secret = Arc::new(items.first().unwrap().to_owned());
        let _ = apply(secret.clone(), data.clone()).await.unwrap();
        let _ = cleanup(secret, data).await.unwrap();

        for ns in namespaces {
            nsapi.delete(ns, &DeleteParams::default()).await.unwrap();
        }
    }
}

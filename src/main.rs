use std::collections::HashSet;
use std::sync::Arc;

use futures::StreamExt;
use k8s_openapi::api::core::v1::{Namespace, Secret};
use kube::api::{ListParams, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::watcher::Config;
use kube::runtime::Controller;
use kube::{Api, Client, Resource, ResourceExt};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::SpanExporter;
use opentelemetry_sdk::trace::TracerProvider;
use tokio::time::Duration;
use tracing::{info, warn};
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use thiserror::Error;
#[derive(Error, Debug)]
pub enum Error {
    #[error("Could not retrieve namespaces: {0}")]
    GetNamespacesFailed(#[source] kube::Error),
    #[error("Could not find labels for secret {name} in namespace {namespace}")]
    MissingLabel { name: String, namespace: String },
    #[error("Could not patch secret: {0}")]
    PatchError(#[source] kube::Error),
}
type Result<T, E = Error> = std::result::Result<T, E>;

struct Data {
    client: Client,
}

const ACTIVE_KEY: &str = "secret-syncer.jeffl.es/sync";
const NAMESPACE_KEY: &str = "secret-syncer.jeffl.es/namespaces";

async fn reconcile(secret: Arc<Secret>, ctx: Arc<Data>) -> Result<Action, Error> {
    let labels = secret.meta().labels.clone();
    let name = secret.name_any();
    let namespace = secret.namespace().unwrap_or(String::from(""));
    // test invariant: that we don't have an ACTIVE_KEY label, I don' think this should happen
    if labels.is_none() || !labels.clone().unwrap().contains_key(ACTIVE_KEY) {
        return Err(Error::MissingLabel { name, namespace });
    }
    let client = &ctx.client;
    let namespaces = Api::<Namespace>::all(client.clone())
        .list(&ListParams::default())
        .await
        .map_err(Error::GetNamespacesFailed)?
        .iter()
        .map(|ns| ns.name_any())
        .collect::<HashSet<String>>();
    let wanted = if let Some(ns) = labels.unwrap().get(NAMESPACE_KEY) {
        ns.split(",").map(String::from).collect::<HashSet<String>>()
    } else {
        warn!("missing namespace list label {NAMESPACE_KEY} on secret {name} in namespace {namespace}, not syncing");
        namespaces.clone()
    };

    let difference = namespaces
        .difference(&wanted)
        .cloned()
        .collect::<Vec<String>>();
    if !difference.is_empty() {
        let unk = difference.join(",");
        warn!("List label {NAMESPACE_KEY} on secret {name} in namespace {namespace} includes unknown namespaces {unk}");
    }
    let union = namespaces.intersection(&wanted);
    for ns in union {
        let api: Api<Secret> = Api::namespaced(client.clone(), ns);
        let owner = secret.controller_owner_ref(&());
        let secret = (*secret).clone();
        let mut meta = secret.metadata.clone();
        meta.owner_references = Some(owner.into_iter().collect());
        meta.namespace = Some(ns.to_string());
        meta.managed_fields = None;
        let dest = Secret {
            data: secret.data,
            immutable: secret.immutable,
            metadata: meta,
            string_data: secret.string_data,
            type_: secret.type_,
        };
        let patch = Patch::Apply(&dest);
        api.patch(&name, &PatchParams::apply("secret-syncer"), &patch)
            .await
            .map_err(Error::PatchError)?;
    }
    Ok(Action::requeue(Duration::from_secs(300)))
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
    let watcher = Config::default().labels(&format!("{ACTIVE_KEY}=true"));
    let api = Api::<Secret>::all(client.clone());
    info!("watching secrets with label {ACTIVE_KEY}=true");
    Controller::new(api, watcher)
        .run(reconcile, error, Arc::new(Data { client }))
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
    use std::sync::Arc;

    use super::{reconcile, Data, ACTIVE_KEY};
    use k8s_openapi::api::core::v1::Secret;
    use kube::{api::ListParams, Api, Client};

    #[tokio::test]
    #[ignore = "uses k8s api"]
    async fn sync_works() {
        let client = Client::try_default().await.unwrap();
        let lp = ListParams::default().labels(&format!("{ACTIVE_KEY}=true"));
        let api: Api<Secret> = Api::namespaced(client.clone(), "source");
        let items = api.list(&lp).await.unwrap().items;
        assert!(items.len() == 1);
        println!("{:#?}", items);
        let data = Arc::new(Data { client });
        let secret = Arc::new(items.first().unwrap().to_owned());
        let _ = reconcile(secret, data).await.unwrap();
    }
}

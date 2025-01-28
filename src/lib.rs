use labels::ACTIVE_LABEL;
use thiserror::Error;

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
    #[error("Could not publish event: {0}")]
    Event(#[source] kube::Error),
}
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn metric_label(&self) -> String {
        format!("{self:?}").to_lowercase()
    }
}

pub mod controller;
pub mod metrics;
pub mod server;
pub mod telemetry;

mod ext;
mod labels;

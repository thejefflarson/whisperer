use kube::runtime::wait;
use thiserror::Error;
use tokio::{sync::watch::error, task::JoinError};

use crate::labels::ACTIVE_LABEL;

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
    #[error("Could not shutdown lock: {0}")]
    Lock(#[source] JoinError),
    #[error("Could not read from channel: {0}")]
    Channel(#[source] error::RecvError),
    #[error("Watch error: {0}")]
    Watch(#[source] wait::Error),
    #[error("Could not create lease: {0}")]
    CreateLease(#[source] kube::Error),
    #[error("Could not get lease: {0}")]
    GetLease(#[source] kube::Error),
    #[error("Could not send")]
    ChannelSend,
}
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn metric_label(&self) -> String {
        format!("{self:?}").to_lowercase()
    }
}

use kube::runtime::wait;
use thiserror::Error;
use tokio::{sync::watch::error, task::JoinError};

#[derive(Error, Debug)]
pub enum Error {
    #[error("Could not retrieve namespaces: {0}")]
    ListNamespaces(#[source] kube::Error),
    #[error("Could not retrieve secret: {0}")]
    GetSecret(#[source] kube::Error),
    #[error("Could not patch secret: {0}")]
    Patch(#[source] kube::Error),
    #[error("Could not patch Whisper status: {0}")]
    PatchStatus(#[source] kube::Error),
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

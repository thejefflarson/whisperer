use std::time::{Duration, Instant};

use futures::{prelude::*, TryFutureExt};
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::chrono::Utc;
use kube::api::{Patch, PatchParams};
use kube::runtime::wait::await_condition;
use kube::{Api, Client};
use tokio::select;
use tokio::sync::oneshot::Sender;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::warn;

use crate::error::{Error, Result};

#[derive(Clone, Debug)]
pub(crate) enum State {
    Leading {
        lease: Lease,
        updated: Instant,
    },
    Following {
        leader: String,
        lease: Lease,
        updated: Instant,
    },
    Standby,
}

impl State {
    fn is_leader(&self) -> bool {
        matches!(self, State::Leading { .. })
    }

    fn leader(&self) -> String {
        match &self {
            State::Leading { .. } => String::from("self"),
            State::Following { leader, .. } => leader.clone(),
            State::Standby => String::from("unknown"),
        }
    }

    fn lease(&self) -> Option<&Lease> {
        match self {
            State::Leading { lease, .. } | State::Following { lease, .. } => Some(lease),
            State::Standby => None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct LeaderLock {
    cancel: Sender<()>,
    handle: JoinHandle<Result<()>>,
}

impl LeaderLock {
    // Effectively destroys this lock, at somepoint this will be an AsyncDrop
    pub async fn retire(self) -> impl Future<Output = Result<Result<()>>> {
        let _ = self.cancel.send(());
        self.handle.map_err(Error::Lock)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LeaderState {
    rx: watch::Receiver<State>,
}
impl LeaderState {
    pub fn is_leader(&self) -> bool {
        self.rx.borrow().is_leader()
    }

    pub fn leader(&self) -> String {
        self.rx.borrow().leader()
    }
}

async fn create(state: State, change: Option<Lease>) -> Result<State> {
    Ok(State::Standby)
}

async fn renew(state: State) -> State {
    State::Standby
}

const LOCK_NAME: &str = "whisperer-controller-lock";
pub(crate) async fn start(client: Client) -> (LeaderState, LeaderLock) {
    let hn = gethostname::gethostname().into_string();
    let hostname = hn.unwrap_or(String::from("unknown"));
    let namespace = client.default_namespace();
    let (state_tx, state_rx) = watch::channel(State::Standby);
    let (cancel_tx, mut cancel_rx) = oneshot::channel();
    let api = Api::<Lease>::namespaced(client.clone(), &namespace);
    // algorithm
    // 1. Ensure the lease exists
    //    a. if not publish Standby and create one with ourselves as a leader
    //       i. publish Leader state to state_tx
    //    b. if it exists and we're not the owner publish Following
    // 2. loop
    //    a. if we're leading renew the lease before the timeout eg every 5 seconds for a 15 second timeout
    //    b. if not check again at jitter * timeout do routine 1.
    // 3. set up a watcher to see if we somehow lose the lease in the meantime
    // conditions to listen for:
    // 1. lease is deleted
    // 2. lease expires
    // 3. lease changes owner
    let handle = tokio::spawn(async move {
        let mut state = State::Standby;

        loop {
            let timer = sleep(Duration::from_secs(5));
            let deleted = await_condition(api.clone(), LOCK_NAME, |lease: Option<&Lease>| {
                lease.is_none()
            });
            let expired = await_condition(api.clone(), LOCK_NAME, |lease: Option<&Lease>| {
                lease
                    .and_then(|lease| lease.spec.clone())
                    .and_then(|spec| spec.renew_time)
                    .map_or(true, |time| time.0 < Utc::now())
            });
            let owner: Option<String> = None;
            let owner_changed = await_condition(api.clone(), LOCK_NAME, |lease: Option<&Lease>| {
                let test = lease
                    .and_then(|lease| lease.spec.clone())
                    .and_then(|spec| spec.holder_identity);
                test == owner
            });
            select! {
                change = deleted => match change {
                    Ok(lease) => {
                        let _ = create(state.clone(), lease).await.map(|it| state = it).map_err(|e| warn!(error = ?e, "Creation error"));
                    },
                    Err(e) => {warn!(error = ?e, "Error on deleted lease change");}
                },
                change = owner_changed => continue,
                change = expired => continue,
                // renew the lease if we're leading else check if we can grab it
                _ = timer => state = renew(state).await,
                // we're done here
                _ = &mut cancel_rx => break
            }
        }
        if let State::Leading { .. } = state {
            let pp = PatchParams::default();
            let patch = Patch::Apply(Lease {
                spec: Some(LeaseSpec {
                    lease_duration_seconds: Some(1),
                    ..Default::default()
                }),
                ..Default::default()
            });
            let _ = api
                .patch(&hostname, &pp, &patch)
                .await
                .map_err(Error::Patch)?;
            state = State::Standby;
            let _ = state_tx.send(state);
        };
        Ok(())
    });
    let lock = LeaderLock {
        cancel: cancel_tx,
        handle,
    };
    let state = LeaderState { rx: state_rx };
    (state, lock)
}

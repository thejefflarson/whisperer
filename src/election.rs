use std::ops::Add;
use std::time::{Duration, Instant};

use futures::prelude::*;
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use kube::api::{Patch, PatchParams};
use kube::runtime::watcher::watch_object;
use kube::{Api, Client};
use tokio::select;
use tokio::sync::oneshot::Sender;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::sleep;

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
const LOCK_NAME: &str = "whisperer-controller-lock";
pub(crate) async fn start(client: Client) -> (LeaderState, LeaderLock) {
    let hn = gethostname::gethostname().into_string();
    let hostname = hn.unwrap_or(String::from("unknown"));
    let namespace = client.default_namespace();
    let (state_tx, state_rx) = watch::channel(State::Standby);
    let (cancel_tx, mut cancel_rx) = oneshot::channel();
    let api = Api::<Lease>::namespaced(client.clone(), &namespace);
    let watcher = watch_object(api.clone(), LOCK_NAME);

    // algorithm
    // 1. Ensure the lease exists
    //    a. if not publish Standby and create one with ourselves as a leader
    //       i. publish Leader state to state_tx
    //    b. if it exists and we're not the owner publish Following
    // 2. loop
    //    a. if we're leading renew the lease before the timeout eg every 5 seconds for a 15 second timeout
    //    b. if not check again at jitter * timeout do routine 1.
    // 3. set up a watcher to see if we somehow lose the lease in the meantime
    let handle = tokio::spawn(async move {
        let mut state = State::Standby;
        tokio::pin!(watcher);

        loop {
            let timer = sleep(Duration::from_secs(5));
            select! {
                // something changed for us
                Some(change) = watcher.next() => continue,
                // renew the lease if we're leading
                _ = timer => continue,
                // we're done here
                _ = &mut cancel_rx => break
            }
        }
        if let State::Leading { .. } = state {
            let now = Instant::now().add(Duration::from_secs(1));
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

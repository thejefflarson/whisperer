use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{prelude::*, TryFutureExt};
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use k8s_openapi::chrono::Utc;
use kube::api::{ObjectMeta, Patch, PatchParams};
use kube::runtime::wait::await_condition;
use kube::{Api, Client};
use tokio::select;
use tokio::sync::oneshot::Sender;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{info, instrument, warn};

use crate::error::{Error, Result};

#[derive(Clone, Debug)]
pub(crate) enum State {
    Leading,
    Following { leader: String },
    Standby,
}

impl State {
    fn is_leader(&self) -> bool {
        matches!(self, State::Leading { .. })
    }

    fn leader(&self) -> String {
        match &self {
            State::Leading { .. } => get_hostname(),
            State::Following { leader, .. } => leader.clone(),
            State::Standby => String::from("unknown"),
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
    pub(crate) fn new(rx: watch::Receiver<State>) -> Self {
        Self { rx }
    }

    pub(crate) fn is_leader(&self) -> bool {
        self.rx.borrow().is_leader()
    }

    pub(crate) fn leader(&self) -> String {
        self.rx.borrow().leader()
    }
}

fn get_hostname() -> String {
    let hn = gethostname::gethostname().into_string();
    hn.unwrap_or(String::from("unknown"))
}

const LEASE_TIME: i32 = 15;
const RENEW_TIME: i32 = LEASE_TIME / 3;
#[instrument]
async fn acquire(_: State, api: Api<Lease>, change: Option<Lease>) -> Result<State> {
    let generations = change
        .and_then(|lease| lease.spec)
        .and_then(|spec| spec.lease_transitions)
        .map_or(0, |generation| generation + 1);
    let hostname = get_hostname();
    let new = Lease {
        metadata: ObjectMeta {
            name: Some(LOCK_NAME.to_string()),
            ..Default::default()
        },
        spec: Some(LeaseSpec {
            holder_identity: Some(hostname.clone()),
            lease_duration_seconds: Some(LEASE_TIME),
            acquire_time: Some(MicroTime(Utc::now())),
            lease_transitions: Some(generations),
            ..Default::default()
        }),
    };
    let _ = api
        .patch(
            LOCK_NAME,
            &PatchParams::apply("whisperer.jeffl.es"),
            &Patch::Apply(&new),
        )
        .await
        .map_err(Error::CreateLease)?;
    info!("{hostname} proposed to lead");
    Ok(State::Leading)
}

#[instrument]
async fn new_owner(state: State, api: Api<Lease>, change: Option<Lease>) -> Result<State> {
    if change.is_none() {
        return acquire(state, api, change).await;
    }
    let lease = change.clone().unwrap();
    let hostname = get_hostname();
    // check if it's us, return state
    if let Some(leader) = lease
        .spec
        .as_ref()
        .and_then(|it| it.holder_identity.clone())
    {
        if leader == hostname {
            info!("{hostname} is confirmed as leader");
            Ok(State::Leading)
        } else {
            info!("{leader} is leader. I am {hostname}");
            Ok(State::Following { leader })
        }
    } else {
        // hm missing holder_identity, try to acquire
        acquire(state, api, change.clone()).await
    }
}

#[instrument]
async fn renew(state: State, api: Api<Lease>, change: Option<Lease>) -> Result<State> {
    if change.is_some() {
        unreachable!("called renew with a change");
    }
    let lease = api.get_opt(LOCK_NAME).await.map_err(Error::GetLease)?;
    let lease = match lease {
        Some(lease) => lease,
        None => return acquire(state, api, None).await,
    };
    let hostname = get_hostname();
    match state {
        State::Leading { .. } | State::Standby => {
            if let Some(leader) = lease
                .spec
                .as_ref()
                .and_then(|it| it.holder_identity.clone())
            {
                if leader != hostname {
                    if matches!(state, State::Standby) {
                        info!("{leader} is leading. I am {hostname}");
                    } else {
                        info!("{hostname} lost lease, {leader} is now leading");
                    }
                    Ok(State::Following { leader })
                } else {
                    // renew lease
                    acquire(state, api, Some(lease)).await
                }
            } else {
                // um, bizarre? no name? no spec? Try and grab it!
                acquire(state, api, Some(lease)).await
            }
        }
        State::Following { .. } => {
            if let Some(spec) = lease.spec.clone() {
                if spec.renew_time.map_or(true, |it| it.0 < Utc::now()) {
                    // jitter for less than 255 milliseconds to reduce contention
                    sleep(Duration::from_millis(rand::random::<u8>().into())).await;
                    // try and grab it!
                    acquire(state, api, Some(lease)).await
                } else {
                    if let Some(leader) = spec.holder_identity {
                        if leader != hostname {
                            info!("{leader} is continuing to lead. I am {hostname}");
                            Ok(State::Following { leader })
                        } else {
                            // not sure this would ever happen? renew it
                            acquire(state, api, Some(lease)).await
                        }
                    } else {
                        // something is really wrong, no leader? Let's grab it.
                        acquire(state, api, Some(lease)).await
                    }
                }
            } else {
                // how would this even happen, no spec? Try and grab it!
                acquire(state, api, Some(lease)).await
            }
        }
    }
}

async fn handle<F, Fut>(
    state: State,
    api: Api<Lease>,
    change: Result<Option<Lease>>,
    operation: F,
    error_msg: &'static str,
) -> State
where
    F: FnOnce(State, Api<Lease>, Option<Lease>) -> Fut,
    Fut: Future<Output = Result<State, Error>>,
{
    match change {
        Ok(lease) => match operation(state.clone(), api, lease).await {
            Ok(new_state) => new_state,
            Err(e) => {
                warn!(error = ?e, error_msg);
                state
            }
        },
        Err(e) => {
            warn!(error = ?e, error_msg);
            state
        }
    }
}

const LOCK_NAME: &str = "whisperer-controller-lock";
pub(crate) async fn start(client: Client) -> (LeaderState, LeaderLock) {
    let (state_tx, state_rx) = watch::channel(State::Standby);
    let (cancel_tx, mut cancel_rx) = oneshot::channel();
    let namespace = client.default_namespace();
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
        let owner: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        loop {
            let timer = sleep(Duration::from_secs(RENEW_TIME as u64));
            let deleted = await_condition(api.clone(), LOCK_NAME, |lease: Option<&Lease>| {
                lease.is_none()
            });
            let owner_changed = await_condition(api.clone(), LOCK_NAME, |lease: Option<&Lease>| {
                let lease = lease
                    .and_then(|lease| lease.spec.clone())
                    .and_then(|spec| spec.holder_identity);
                let mut owner = owner.lock().unwrap();
                if *owner != lease {
                    *owner = lease;
                    true
                } else {
                    false
                }
            });
            select! {
                change = deleted => {
                    state = handle(
                        state,
                        api.clone(),
                        change.map_err(Error::Watch),
                        acquire,
                        "Lease creation failed"
                    ).await;
                    state_tx.send(state.clone()).unwrap();
                },
                change = owner_changed => {
                    state = handle(
                        state,
                        api.clone(),
                        change.map_err(Error::Watch),
                        new_owner,
                        "Owner change operation failed"
                    ).await;
                    state_tx.send(state.clone()).unwrap();
                },
                _ = timer => {
                    state = handle(
                        state,
                        api.clone(),
                        Ok(None),
                        renew,
                        "Timer operation failed"
                    ).await;
                    state_tx.send(state.clone()).unwrap();
                }
                // we're done here
                _ = &mut cancel_rx => break
            }
        }
        // we're shutting down, cleanup our lease
        if let State::Leading { .. } = state {
            let hn = gethostname::gethostname().into_string();
            let hostname = hn.unwrap_or(String::from("unknown"));
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
    let state = LeaderState::new(state_rx);
    (state, lock)
}

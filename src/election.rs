use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::prelude::*;
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use k8s_openapi::chrono::Utc;
use kube::api::{ObjectMeta, Patch, PatchParams, PostParams};
use kube::runtime::wait::await_condition;
use kube::{Api, Client};
use tokio::select;
use tokio::sync::oneshot::Sender;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{info, instrument, warn};

use crate::error::{Error, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
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
            State::Leading => get_hostname(),
            State::Following { leader } => leader.clone(),
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
    pub async fn retire(self) -> Result<()> {
        self.cancel.send(()).map_err(|_| Error::ChannelSend)?;
        match self.handle.await {
            Ok(result) => result,
            Err(join_error) => Err(Error::Lock(join_error)),
        }
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
#[instrument(skip(api, change))]
async fn acquire(_: State, api: Api<Lease>, change: Option<Lease>) -> Result<State> {
    let generations = change
        .clone()
        .and_then(|lease| lease.spec)
        .and_then(|spec| spec.lease_transitions)
        .map_or(0, |generation| generation + 1);
    let acquire_time = change
        .clone()
        .and_then(|lease| lease.spec)
        .and_then(|spec| spec.acquire_time)
        .unwrap_or_else(|| MicroTime(Utc::now()));
    let resource_version = change
        .clone()
        .map(|lease| lease.metadata)
        .and_then(|meta| meta.resource_version);
    let hostname = get_hostname();
    let new = Lease {
        metadata: ObjectMeta {
            name: Some(LOCK_NAME.to_string()),
            resource_version,
            ..Default::default()
        },
        spec: Some(LeaseSpec {
            holder_identity: Some(hostname.clone()),
            lease_duration_seconds: Some(LEASE_TIME),
            acquire_time: Some(acquire_time),
            renew_time: Some(MicroTime(Utc::now())),
            lease_transitions: Some(generations),
            ..Default::default()
        }),
    };
    if change.is_some() {
        api.replace(
            LOCK_NAME,
            &PostParams {
                dry_run: false,
                field_manager: Some(String::from("whisperer.jeffl.es")),
            },
            &new,
        )
        .await
        .map_err(Error::CreateLease)?;
    } else {
        api.create(
            &PostParams {
                dry_run: false,
                field_manager: Some(String::from("whisperer.jeffl.es")),
            },
            &new,
        )
        .await
        .map_err(Error::CreateLease)?;
    }
    info!("{hostname} is leading");
    Ok(State::Leading)
}

#[instrument(skip(api, change))]
async fn new_owner(state: State, api: Api<Lease>, change: Option<Lease>) -> Result<State> {
    // it's been deleted
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
        acquire(state, api, None).await
    }
}

#[instrument(skip(api, change))]
async fn renew(state: State, api: Api<Lease>, change: Option<Lease>) -> Result<State> {
    if change.is_some() {
        unreachable!("called renew with a change");
    }
    let lease = match api.get_opt(LOCK_NAME).await.map_err(Error::GetLease)? {
        Some(lease) => lease,
        None => return acquire(state, api, None).await,
    };
    let current_leader = lease
        .spec
        .as_ref()
        .and_then(|it| it.holder_identity.clone());
    let hostname = get_hostname();
    match (state.clone(), current_leader) {
        // If we're leading but someone else has the lease now
        (State::Leading, Some(leader)) if leader != hostname => {
            info!("{hostname} lost lease, {leader} is now leading");
            Ok(State::Following { leader })
        }

        // If we're in standby and there's a leader
        (State::Standby, Some(leader)) if leader != hostname => {
            info!("{leader} is leading. I am {hostname}");
            Ok(State::Following { leader })
        }

        // If we're following and should try to acquire the lease
        (State::Following { .. }, Some(leader)) => {
            let should_attempt_acquisition = lease
                .spec
                .clone()
                .and_then(|spec| Some((spec.renew_time, spec.lease_duration_seconds)))
                .and_then(|pair| match pair {
                    (Some(microtime), Some(seconds)) => {
                        Some(microtime.0 + Duration::from_secs(seconds as u64))
                    }
                    (Some(microtime), None) => Some(microtime.0),
                    _ => None,
                })
                .map_or(true, |time| time < Utc::now());

            if should_attempt_acquisition {
                // Add jitter to reduce contention
                sleep(Duration::from_millis(rand::random::<u8>().into())).await;
                acquire(state, api, Some(lease)).await
            } else if leader != hostname {
                info!("{leader} is continuing to lead. I am {hostname}");
                Ok(State::Following { leader })
            } else {
                // This case is rare but possible - we thought we were following but actually we're
                // the leader, or maybe our lease is gone?
                acquire(state, api, Some(lease)).await
            }
        }

        // Default case: try to acquire the lease
        _ => acquire(state, api, Some(lease)).await,
    }
}

const BACKOFF: u64 = 30;
async fn transition<F, Fut>(
    current_state: State,
    api: Api<Lease>,
    lease_result: Result<Option<Lease>>,
    operation: F,
    context: &str,
) -> (State, bool)
where
    F: FnOnce(State, Api<Lease>, Option<Lease>) -> Fut,
    Fut: Future<Output = Result<State, Error>>,
{
    let new_state = {
        // Extract the lease or handle error and return current state
        let lease = match lease_result {
            Ok(lease) => lease,
            Err(err) => {
                warn!(error = ?err, context = context, "Lease operation failed");
                sleep(Duration::from_secs(BACKOFF)).await;
                return (current_state, false);
            }
        };

        // Perform the operation or handle error and return current state
        match operation(current_state.clone(), api, lease).await {
            Ok(new_state) => new_state,
            Err(err) => {
                warn!(error = ?err, context = context, "State transition failed");
                sleep(Duration::from_secs(BACKOFF)).await;
                current_state.clone()
            }
        }
    };

    let changed = current_state != new_state;
    (new_state, changed)
}

const LOCK_NAME: &str = "whisperer-controller-lock";
pub(crate) async fn start(client: Client) -> (LeaderState, LeaderLock) {
    let (state_tx, state_rx) = watch::channel(State::Standby);
    let (cancel_tx, mut cancel_rx) = oneshot::channel();
    let namespace = client.default_namespace();
    let api = Api::<Lease>::namespaced(client.clone(), &namespace);
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
                    let (new_state, changed) = transition(
                        state,
                        api.clone(),
                        change.map_err(Error::Watch),
                        acquire,
                        "Lease creation failed"
                    ).await;
                    state = new_state;
                    if changed {
                        state_tx.send(state.clone()).map_err(|_| Error::ChannelSend)?;
                    }
                },
                change = owner_changed => {
                    let (new_state, changed) = transition(
                        state,
                        api.clone(),
                        change.map_err(Error::Watch),
                        new_owner,
                        "Owner change operation failed"
                    ).await;
                    state = new_state;
                    if changed {
                        state_tx.send(state.clone()).map_err(|_| Error::ChannelSend)?;
                    }
                },
                _ = timer => {
                    let (new_state, changed) = transition(
                        state,
                        api.clone(),
                        Ok(None),
                        renew,
                        "Timer operation failed"
                    ).await;
                    state = new_state;
                    if changed {
                        state_tx.send(state.clone()).map_err(|_| Error::ChannelSend)?;
                    }
                }
                // we're done here
                _ = &mut cancel_rx => break
            }
        }
        // we're shutting down, cleanup our lease
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
                .patch(&LOCK_NAME, &pp, &patch)
                .await
                .map_err(Error::Patch)?;
            state_tx
                .send(State::Standby)
                .map_err(|_| Error::ChannelSend)?;
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

#[cfg(test)]
mod test {
    use std::time::Duration;

    use httpmock::prelude::*;
    use httpmock::{Then, When};
    use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
    use k8s_openapi::chrono::Utc;
    use kube::api::ObjectMeta;
    use kube::{Api, Client, Config};
    use serde_json::json;

    use crate::election::{get_hostname, LEASE_TIME};

    use super::{new_owner, renew, State};

    #[tokio::test]
    async fn test_new_owner() {
        let server = MockServer::start_async().await;
        let _ = server.mock(|when: When, then: Then| {
            when.any_request();
            then.status(200).json_body(json!(Lease::default()));
        });
        let client = Client::try_from(Config::new(server.url("/").parse().unwrap())).unwrap();
        let namespace = client.default_namespace();
        let api = Api::<Lease>::namespaced(client.clone(), &namespace);
        let leader = get_hostname();
        let state = new_owner(
            State::Following {
                leader: leader.clone(),
            },
            api.clone(),
            Some(Lease {
                spec: Some(LeaseSpec {
                    holder_identity: Some(leader),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        assert_eq!(state, State::Leading);

        let state = new_owner(
            State::Leading,
            api,
            Some(Lease {
                spec: Some(LeaseSpec {
                    holder_identity: Some(String::from("other")),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            state,
            State::Following {
                leader: String::from("other")
            }
        );
    }

    async fn create_server() -> (MockServer, Api<Lease>) {
        let server = MockServer::start_async().await;
        let client = Client::try_from(Config::new(server.url("/").parse().unwrap())).unwrap();
        let namespace = client.default_namespace();
        let api = Api::<Lease>::namespaced(client.clone(), &namespace);
        (server, api)
    }

    #[tokio::test]
    async fn test_leading_following() {
        let (server, api) = create_server().await;
        let recording = server.record(|rule| {
            rule.filter(|when| {
                when.any_request(); // Record all requests
            });
        });

        // test leading -> following
        let mock = server.mock(|when: When, then: Then| {
            when.method(GET).path(
                "/apis/coordination.k8s.io/v1/namespaces/default/leases/whisperer-controller-lock",
            );
            let lease = Lease {
                spec: Some(LeaseSpec {
                    holder_identity: Some("not-us".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            then.status(200).json_body(json!(lease));
        });
        let state = renew(State::Leading, api.clone(), None).await.unwrap();
        assert_eq!(
            state,
            State::Following {
                leader: "not-us".to_string()
            }
        );
        mock.assert();
        let _ = recording
            .save_to_async("recordings", "leading_to_following")
            .await
            .unwrap();
    }

    #[tokio::test]
    /// test standby -> following
    async fn test_standby_to_following() {
        let (server, api) = create_server().await;
        let recording = server.record(|rule| {
            rule.filter(|when| {
                when.any_request(); // Record all requests
            });
        });
        let mock = server.mock(|when: When, then: Then| {
            when.method(GET).path(
                "/apis/coordination.k8s.io/v1/namespaces/default/leases/whisperer-controller-lock",
            );
            let lease = Lease {
                spec: Some(LeaseSpec {
                    holder_identity: Some("not-us".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            then.status(200).json_body(json!(lease));
        });
        let state = renew(State::Standby, api.clone(), None).await.unwrap();
        assert_eq!(
            state,
            State::Following {
                leader: "not-us".to_string()
            }
        );
        mock.assert();
        let _ = recording
            .save_to_async("recordings", "standby_to_following")
            .await
            .unwrap();
    }

    #[tokio::test]
    /// test promotion to leading with a malformed lease
    async fn test_following_to_leading_malformed() {
        let (server, api) = create_server().await;
        let recording = server.record(|rule| {
            rule.filter(|when| {
                when.any_request(); // Record all requests
            });
        });
        // case 1. no expires
        let mock = server.mock(|when: When, then: Then| {
            when.method(GET).path(
                "/apis/coordination.k8s.io/v1/namespaces/default/leases/whisperer-controller-lock",
            );
            let lease = Lease {
                spec: Some(LeaseSpec {
                    holder_identity: Some("not-us".to_string()),
                    ..Default::default()
                }),
                metadata: ObjectMeta {
                    resource_version: Some(String::from("1")),
                    ..Default::default()
                },
            };
            then.status(200).json_body(json!(lease));
        });

        let put = server.mock(|when, then| {
            when.method(PUT).path(
                "/apis/coordination.k8s.io/v1/namespaces/default/leases/whisperer-controller-lock",
            );
            let lease = Lease {
                spec: Some(LeaseSpec {
                    holder_identity: Some(get_hostname().to_string()),
                    ..Default::default()
                }),
                metadata: ObjectMeta {
                    resource_version: Some(String::from("1")),
                    ..Default::default()
                },
            };
            then.status(200).json_body(json!(lease));
        });
        let state = renew(
            State::Following {
                leader: "not-us".to_string(),
            },
            api.clone(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(state, State::Leading);
        mock.assert();
        put.assert();
        let _ = recording
            .save_to_async("recordings", "following_to_leading_malformed")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_following_to_leading() {
        let (server, api) = create_server().await;
        let recording = server.record(|rule| {
            rule.filter(|when| {
                when.any_request(); // Record all requests
            });
        });
        // case 2. expired
        let mock = server.mock(|when: When, then: Then| {
            when.method(GET).path(
                "/apis/coordination.k8s.io/v1/namespaces/default/leases/whisperer-controller-lock",
            );
            let lease = Lease {
                spec: Some(LeaseSpec {
                    holder_identity: Some("not-us".to_string()),
                    lease_duration_seconds: Some(1),
                    acquire_time: Some(MicroTime(Utc::now() - Duration::from_secs(2))),
                    renew_time: Some(MicroTime(Utc::now() - Duration::from_secs(2))),
                    lease_transitions: Some(0),
                    ..Default::default()
                }),
                ..Default::default()
            };
            then.status(200).json_body(json!(lease));
        });

        let patch = server.mock(|when, then| {
            let patch = Lease {
                spec: Some(LeaseSpec {
                    holder_identity: Some(get_hostname().to_string()),
                    lease_duration_seconds: Some(LEASE_TIME),
                    lease_transitions: Some(1),
                    ..Default::default()
                }),
                metadata: ObjectMeta {
                    ..Default::default()
                },
            };
            when.method(PUT).path(
                "/apis/coordination.k8s.io/v1/namespaces/default/leases/whisperer-controller-lock",
            ).json_body_includes(json!(patch).to_string());
            let lease = Lease {
                spec: Some(LeaseSpec {
                    holder_identity: Some(get_hostname().to_string()),
                    lease_transitions: Some(1),
                    ..Default::default()
                }),
                ..Default::default()
            };
            then.status(200).json_body(json!(lease));
        });
        let state = renew(
            State::Following {
                leader: "not-us".to_string(),
            },
            api.clone(),
            None,
        )
        .await;

        let _ = recording
            .save_to_async("recordings", "following_to_leading")
            .await
            .unwrap();
        mock.assert();
        patch.assert();
        assert_eq!(state.unwrap(), State::Leading);
    }

    #[tokio::test]
    /// test standby -> leading
    async fn test_standby_to_leading() {
        let (server, api) = create_server().await;
        let recording = server.record(|rule| {
            rule.filter(|when| {
                when.any_request(); // Record all requests
            });
        });
        let mock = server.mock(|when: When, then: Then| {
            when.method(GET).path(
                "/apis/coordination.k8s.io/v1/namespaces/default/leases/whisperer-controller-lock",
            );
            let lease = Lease {
                spec: Some(LeaseSpec {
                    holder_identity: Some(get_hostname()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            then.status(200).json_body(json!(lease));
        });

        let patch = server.mock(|when, then| {
            when.method(PUT).path(
                "/apis/coordination.k8s.io/v1/namespaces/default/leases/whisperer-controller-lock",
            ); // todo test the body is the right shape too
            let lease = Lease {
                spec: Some(LeaseSpec {
                    holder_identity: Some(get_hostname().to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            then.status(200).json_body(json!(lease));
        });
        let state = renew(State::Standby, api.clone(), None).await.unwrap();
        assert_eq!(state, State::Leading);
        patch.assert();
        mock.assert();
        let _ = recording
            .save_to_async("recordings", "standby_to_leading")
            .await
            .unwrap();
    }

    #[tokio::test]
    /// test leading -> leading
    async fn test_leading_leading() {
        let (server, api) = create_server().await;
        let recording = server.record(|rule| {
            rule.filter(|when| {
                when.any_request(); // Record all requests
            });
        });
        let mock = server.mock(|when: When, then: Then| {
            when.method(GET).path(
                "/apis/coordination.k8s.io/v1/namespaces/default/leases/whisperer-controller-lock",
            );
            let lease = Lease {
                spec: Some(LeaseSpec {
                    holder_identity: Some(get_hostname()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            then.status(200).json_body(json!(lease));
        });

        let patch = server.mock(|when, then| {
            when.method(PUT).path(
                "/apis/coordination.k8s.io/v1/namespaces/default/leases/whisperer-controller-lock",
            ); // todo test the body is the right shape too
            let lease = Lease {
                spec: Some(LeaseSpec {
                    holder_identity: Some(get_hostname().to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            then.status(200).json_body(json!(lease));
        });
        let state = renew(State::Leading, api.clone(), None).await.unwrap();
        assert_eq!(state, State::Leading);
        patch.assert();
        mock.assert();
        let _ = recording
            .save_to_async("recordings", "leading_to_leading")
            .await
            .unwrap();
    }
}

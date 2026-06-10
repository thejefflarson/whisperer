# Whisper CRD redesign — session handoff

Branch: **`feat/whisper-crd`** (in this repo, `~/dev/whisperer`).
Status: **in progress, does NOT compile yet** (controller rewrite half-applied — see "Resume here").

## Why we're doing this

whisperer today is driven by **labels + annotations on the Secret** (`whisperer.jeffl.es/sync=true`
+ `whisperer.jeffl.es/namespaces=…`). Because the sync config lives *on the secret*, the operator
must **`list`/`watch` every Secret in the cluster** to discover labelled ones — that cluster-wide
secret read is the operator's biggest privilege (its ClusterRole has get/list/watch/create/update/
patch/delete on secrets in **all** namespaces). In a cluster that's otherwise fanatically
least-privilege, that's the one "god mode" grant.

This came up while wiring Argo CD Image Updater in the `~/dev/cluster` repo: image-updater needs the
`github` ghcr pull secret in the `argocd` namespace, which whisperer replicates. (That side is DONE
and working — image-updater PRs #40/#41/#42 merged; `argocd` was added to `githubSyncNamespaces`.)
The discussion then turned to whisperer's RBAC, and we chose to **move config to a CRD** ("option b —
more robust / less hacky") because it removes the cluster-wide secret `list/watch` *structurally*:
the operator watches its own `Whisper` resources instead of every Secret.

## Design decisions (the ADRs for this change)

1. **`Whisper` CRD replaces label-on-Secret config.** Group `whisperer.jeffl.es`, version `v1`,
   **namespaced**, status subresource.
2. **A `Whisper` lives in the SOURCE namespace** and names a Secret in *that same* namespace
   (`spec.secretName`). This keeps the authorization boundary local — you can only whisper a secret
   out of a namespace where you can already create objects, so nobody replicates a secret they don't
   own. (Mirrors the old "source must be labelled" guard.)
3. **Targets still consent** via the `whisperer.jeffl.es/allow-sync=true` namespace label; protected
   namespaces (`kube-system`/`kube-public`/`kube-node-lease`/operator's own) are always skipped.
   `resolve_targets()` / `is_consenting_namespace()` are unchanged and reused.
4. **`status.syncedNamespaces` tracks where copies currently live.** Orphan cleanup diffs status vs.
   current targets and deletes by name in those *specific* namespaces — so there's **no cluster-wide
   secret `list`** for orphan discovery (the old `Api::<Secret>::all().list()` is gone).
5. **Source-secret watch is SCOPED to the operator's own namespace** (`client.default_namespace()`),
   mapped back to the Whisper(s) referencing it via the controller's Whisper store. → instant
   propagation **without** cluster-wide secret watch. Requeue stays long (600s) purely as drift
   insurance. (This is the refinement of the user's "drop the requeue down?" — instead of a shorter
   blanket requeue, we get watch-driven instant updates with a tiny namespaced Role.)
6. **Cleanup deletes COPIES only, not the source secret.** Behaviour change from the old model (where
   the labelled source Secret *was* the finalized object and got deleted on removal). Now the Whisper
   is just a sync *declaration*; deleting it removes copies and leaves the user's source secret alone.
7. **Clean cutover** — drop the label model entirely (few consumers; it's our operator).

### Resulting RBAC (the whole point)

| Resource | Verbs | Scope |
|----------|-------|-------|
| `whispers.whisperer.jeffl.es` (+ `/status`) | get/list/watch (+ patch status) | cluster-wide — *own API group, not secret access* |
| secrets | get/list/watch | **operator namespace only** (a `Role`) — for source reads + scoped watch |
| secrets | create/update/patch/delete | **target namespaces only** (`RoleBinding` per target, driven by a `writeNamespaces` values list) |
| namespaces | get/list/watch | cluster-wide (metadata, low-risk) |
| events | create/patch | (decide: cluster-wide is low-risk, or scope to targets+operator ns) |
| leases | … | operator namespace (leader election, unchanged) |

→ **No cluster-wide secret access at all.** Both the write-scoping ("Tier 1") and the read-scoping
("Tier 2") land together in this redesign.

## What's DONE

- **Deps added** (Cargo.toml): `kube` gained the `derive` feature; added `serde` (derive),
  `schemars` 1.2.1, and moved `serde_json` to main deps (the `CustomResource` derive needs it).
- **`src/whisper.rs`** — the `Whisper` CustomResource. `WhisperSpec { secret_name, namespaces }`,
  `WhisperStatus { synced_namespaces }`. Compiles. Wired into `src/lib.rs` (`pub mod whisper;`).
- **`src/controller.rs` — partially rewritten:**
  - imports: added `whisper::Whisper`, `serde_json::json`.
  - `secret_namespaces(...)` → replaced with **`resolve(wanted: &NSSet, client: &Client)`**.
  - **`apply`** rewritten to take `Arc<Whisper>`: reads the source secret from the Whisper's own ns,
    resolves targets, reclaims orphans by diffing `status.syncedNamespaces` (no cluster-wide list),
    writes copies via SSA, then calls `record_synced`.
  - added **`record_synced(...)`** (patches the status subresource, `Patch::Merge`).
  - **Kept unchanged:** `Context`, `record`, `is_protected_namespace`, `is_managed_copy`,
    `is_consenting_namespace`, `resolve_targets`, `TargetResolution`, `NSSet`, `delete`, `delete_copy`,
    `SecretExt::dup` (in `src/ext.rs`).

## Resume here — finish the controller rewrite

`cleanup`, `dispatcher`, `error`, and `run` are **still the old `Secret`-keyed versions** and call
`apply(secret)` — so the crate does **not** compile. Replace them with the versions below, add the
`Error::PatchStatus` variant, then `cargo check`.

### 1. `src/error.rs` — add a variant (and clean up dead ones)

Add:
```rust
#[error("Could not patch Whisper status: {0}")]
PatchStatus(#[source] kube::Error),
```
Then remove the now-unused `MissingLabel` and `MissingDestinationAnnotation` variants (apply no longer
uses them). Watch for unused-import/const warnings (CLAUDE.md treats warnings as errors): `error!`
(tracing) may now be unused in controller.rs; `ACTIVE_LABEL` / `NAMESPACE_ANNOTATION` may only be used
by tests after this.

### 2. `src/controller.rs` — replace `cleanup` … through end of `run`

```rust
/// Finalizer cleanup: a Whisper is being deleted, so reclaim every copy it made
/// — the namespaces in `status.syncedNamespaces` plus the current spec targets,
/// in case status lagged. The *source* secret is left alone; it's the user's.
#[instrument(skip(ctx, whisper))]
async fn cleanup(whisper: Arc<Whisper>, ctx: Arc<Context>) -> Result<Action> {
    let secret_name = whisper.spec.secret_name.clone();
    let mut copies = whisper
        .status
        .as_ref()
        .map(|s| s.synced_namespaces.iter().cloned().collect::<NSSet>())
        .unwrap_or_default();
    copies.extend(whisper.spec.namespaces.iter().cloned());
    let namespaces = copies.iter().cloned().collect::<Vec<_>>().join(", ");
    ctx.record(
        &Notice {
            type_: EventType::Normal,
            reason: "Delete Requested".into(),
            note: Some(format!("Reclaiming '{secret_name}' from {namespaces}")),
            action: "Delete".into(),
            secondary: None,
        },
        &whisper.object_ref(&()),
    )
    .await?;
    for ns in copies {
        delete_copy(secret_name.clone(), ns, ctx.clone()).await?;
    }
    Ok(Action::await_change())
}

#[instrument(skip(ctx))]
async fn dispatcher(whisper: Arc<Whisper>, ctx: Arc<Context>) -> Result<Action> {
    if !ctx.state.is_leader() {
        info!("not leader (leader is {}), ignoring change", ctx.state.leader());
        return Ok(Action::await_change());
    }
    let metrics = ctx.metrics.clone();
    let namespace = whisper.namespace().unwrap_or_default();
    let _record = metrics.duration();
    let api: Api<Whisper> = Api::namespaced(ctx.client.clone(), &namespace);
    finalizer(&api, FINALIZER, whisper, |event| async {
        metrics.reconcile();
        match event {
            Event::Apply(whisper) => apply(whisper, ctx).await,
            Event::Cleanup(whisper) => cleanup(whisper, ctx).await,
        }
    })
    .await
    .map_err(|e| {
        metrics.failure();
        Error::Finalizer(Box::new(e))
    })
}

fn error(_object: Arc<Whisper>, error: &Error, _ctx: Arc<Context>) -> Action {
    warn!(error = ?error, "Requeueing after error");
    Action::requeue(Duration::from_secs(1))
}

pub async fn run(metrics: MetricState) {
    let client = Client::try_default().await.expect("could not connect to k8s");
    let (state, lock) = start(client.clone()).await;

    // Source secrets live in the operator's own namespace; watch only there so we
    // never need cluster-wide secret list/watch. The root watch is on Whispers
    // (our own API group), and a changed source secret maps back to the Whisper(s)
    // that reference it via the controller's Whisper store.
    let source_ns = client.default_namespace().to_string();
    let whispers = Api::<Whisper>::all(client.clone());
    let secrets = Api::<Secret>::namespaced(client.clone(), &source_ns);
    info!("watching Whispers; source secrets in namespace '{source_ns}'");
    let recorder = Recorder::new(
        client.clone(),
        Reporter {
            controller: "whisperer".into(),
            instance: env::var("CONTROLLER_POD_NAME").ok(),
        },
    );
    let controller = Controller::new(whispers, Config::default());
    let store = controller.store();
    controller
        .watches(secrets, Config::default(), move |secret| {
            let name = secret.name_any();
            let ns = secret.namespace();
            store
                .state()
                .into_iter()
                .filter(|w| w.spec.secret_name == name && w.namespace() == ns)
                .map(|w| ObjectRef::from_obj(w.as_ref()))
                .collect::<Vec<_>>()
        })
        .shutdown_on_signal()
        .run(
            dispatcher,
            error,
            Arc::new(Context { client, recorder, metrics, state }),
        )
        .for_each(|res| async move {
            match res {
                Ok(o) => info!("reconciled {:?}", o),
                Err(RuntimeError::ObjectNotFound(_)) => info!("object already deleted"),
                Err(e) => warn!(error = ?e, "reconcile failed"),
            }
        })
        .await;
    let _ = lock.retire().await;
}
```

**Things to verify on first `cargo check`:**
- `Controller::store()` returns `Store<Whisper>`; `store.state()` → `Vec<Arc<Whisper>>`. Confirm the
  method names against the installed kube-rs 3.1 API (might be `.store()` vs needing a manual
  reflector — adjust if the borrow checker / API disagrees).
- `ObjectRef::from_obj(w.as_ref())` for a namespaced `Whisper` — confirm signature.
- `whisper.object_ref(&())` works (Whisper derives `Resource` with `DynamicType = ()`).
- The `.watches()` mapper closure must be `Fn + Send + 'static`; `store` captured by move.

## TODO (ordered)

- [ ] **Finish controller** (above) + `Error::PatchStatus`; `cargo check` clean (no warnings).
- [ ] **Rewrite tests** — `src/controller.rs`'s `#[cfg(test)] mod test` (~290 lines) tests the old
      Secret/label `apply`; rewrite for the Whisper model (httpmock is already a dev-dep). `cargo test`.
      Keep the pure `resolve_targets` tests (unchanged).
- [ ] **CRD manifest** — emit `Whisper::crd()` YAML. Check for an existing crdgen pattern; if none,
      add a small `src/bin/crdgen.rs` (or a test that writes it) and commit the CRD into the chart.
- [ ] **Chart RBAC** — `chart/templates/rbac.yaml`: implement the scoped model in the table above
      (read ClusterRole w/o secrets; operator-ns secret-read Role; `secret-writer` ClusterRole +
      per-target RoleBindings from a new `writeNamespaces` values list; keep leader-election Role).
      Add `writeNamespaces: []` to `chart/values.yaml`. Decide the events scope.
- [ ] **Chart CRD install** — add the Whisper CRD to the chart (`chart/templates/` or `crds/`).
- [ ] **README** — rewrite the usage section for the `Whisper` CRD (replaces the label/annotation
      walkthrough), **kill the stale "maybe hold off? … a few days away from CICD and a helm chart"
      line (line ~10)**, and add a **Permissions** section documenting the scoped RBAC. Keep the
      playful voice. (This is the "refresh the README" ask — it rides here because the usage model
      changed.)
- [ ] **Release** — bump `Cargo.toml` version + `chart/Chart.yaml` (`version` 0.2.4→, `appVersion`
      0.2.6→); CI builds/signs the image.

## Then: migrate the cluster (separate repo, `~/dev/cluster`)

The cluster runs a **vendored copy** of this chart at `cluster/charts/whisperer/` (its own
`rbac.yaml`/`deployment.yaml` + cluster-specific `github-source.yaml` + `target-namespaces.yaml`).
After the whisperer release:

- [ ] Re-vendor the chart changes (CRD, scoped RBAC) from `chart/` into `cluster/charts/whisperer/`.
- [ ] Bump the whisperer image tag in `cluster/charts/whisperer/values.yaml`.
- [ ] Replace the source Secret's `whisperer.jeffl.es/sync` label + `namespaces` annotation in
      `cluster/charts/whisperer/templates/github-source.yaml` with a **`Whisper` CR**
      (secretName: github, namespaces: analytics,watcher,dev,public,argocd).
- [ ] Set `writeNamespaces` = `analytics,watcher,dev,public,argocd` (same set as
      `githubSyncNamespaces`).
- [ ] Drop the cluster-wide secret-write RBAC.
- [ ] **Validate:** `github` secret still syncs into all five namespaces; Argo CD Image Updater still
      authenticates to ghcr (it reads `pullsecret:argocd/github`); `python3 scripts/authz-audit.py`
      green; `kubectl get whispers -A` healthy.

## Tradeoffs to remember
- Status is the source of truth for copy locations. If `status.syncedNamespaces` is ever lost, the
  operator won't know about pre-existing copies (no cluster-wide list to rediscover them) — manual
  cleanup in that edge case. Acceptable for the RBAC win; note it in the README.
- Source secrets must live in the operator's namespace (that's where the scoped watch + read Role
  are). True for this cluster (the `github` source is in the whisperer ns). If you ever want sources
  elsewhere, make the watched/source namespace set configurable and extend the read Role/RoleBindings.

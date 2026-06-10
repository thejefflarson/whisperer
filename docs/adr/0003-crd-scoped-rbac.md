# 3. Whisper CRD and scoped RBAC

Date: 2026-06-09

Status: Accepted

## Context

Until now, sync config lived **on the Secret**: a source carried
`whisperer.jeffl.es/sync=true` and a `whisperer.jeffl.es/namespaces=…` annotation.
Because the configuration was on the secret, the operator had to **`list`/`watch`
every Secret in the cluster** to discover the labelled ones. That forced its
ClusterRole to hold `get/list/watch/create/update/patch/delete` on **secrets in
all namespaces** — cluster-wide read of every secret, the single largest privilege
in an otherwise least-privilege deployment.

This surfaced while wiring Argo CD Image Updater (which needs the ghcr pull secret
replicated into the `argocd` namespace): the discussion turned to whisperer's RBAC
and the cluster-wide secret read it depends on structurally.

## Decision

Replace the label-on-Secret model with a namespaced **`Whisper` CRD** (group
`whisperer.jeffl.es`, version `v1`, status subresource). A `Whisper` lives in the
**source** namespace and names a Secret in that same namespace
(`spec.secretName`) plus its target namespaces (`spec.namespaces`).

This removes cluster-wide secret access **structurally**:

- The operator's root watch is on **Whispers** (its own API group), not Secrets.
- Source secrets are read only in the **operator's own namespace**, and the
  source-secret watch is scoped there too (`Api::<Secret>::namespaced`), mapped
  back to referencing Whispers via the controller's store — instant propagation
  with only a namespaced secret-read Role.
- Where copies currently live is tracked in **`status.syncedNamespaces`**, and
  also rediscoverable by **name-probing the `writeNamespaces`** with a `get` (see
  "Orphan recovery" below). Orphan cleanup diffs that view against the current
  targets and deletes by name in those specific namespaces — so there is **no
  cluster-wide secret `list`** for orphan discovery.
- Writes are confined to an explicit `writeNamespaces` Helm value: one
  secret-writer `RoleBinding` per target namespace, never a cluster-wide grant.

Authorization is otherwise unchanged from [ADR 0001](0001-cross-namespace-sync-authorization.md):
targets still consent with `whisperer.jeffl.es/allow-sync=true`, protected
namespaces are still denied, and cross-namespace deletes require the operator's
copy markers plus the verified object's delete preconditions — bounded, crucially,
by RBAC confining deletes to the write/drain namespaces (see "Deployment
assumptions" below; the markers themselves are best-effort, not unforgeable).

Cleanup behaviour changes deliberately: a `Whisper` is a sync *declaration*, so
deleting it removes the **copies** and leaves the user's source secret alone (the
old model finalized and deleted the labelled source Secret itself).

### Resulting RBAC

| Resource | Verbs | Scope |
|----------|-------|-------|
| `whispers` (+`/status`, `/finalizers`) | get/list/watch/update/patch | cluster-wide — our own API group, not secret access |
| secrets | get/list/watch | operator namespace only (a `Role`) |
| secrets | get/create/update/patch/delete | target namespaces only (a `RoleBinding` per `writeNamespaces` entry) |
| secrets | get/delete | draining namespaces only (a `RoleBinding` per `drainNamespaces` entry) |
| namespaces | get/list/watch | cluster-wide (metadata, low-risk) |
| events | create/patch | operator namespace (Whispers are recorded against, and live in, that namespace) |
| leases | get/list/watch/create/update/patch/delete | operator namespace (leader election, unchanged) |

**No cluster-wide secret access remains.**

## Orphan recovery (and its threat model)

Dropping the cluster-wide secret `list` raised a question: if a Whisper's
`status.syncedNamespaces` is ever lost (the resource is recreated from a backup
that omits status, say), how does the operator rediscover the copies it already
made? An early version of this ADR accepted "manual cleanup in that edge case" as
the cost. We can do better — without giving the cluster-wide read back.

**The mechanism.** The operator also rediscovers copies by **name-probing the
`writeNamespaces`** (`discover_copies` in `controller.rs`): for each namespace in
that set, `get` the copy by name and keep it if it's a managed copy
([`is_owned_copy`]) owned by this Whisper (its `whisperer.jeffl.es/owner-uid`
label matches). Two facts make this a *complete* substitute for the lost ledger:

1. Copy names are **stable** — a copy is named after the **Whisper**
   (`metadata.name`), not the mutable `spec.secretName` (see "Stable copy naming")
   — so the probe name is known a-priori and never drifts.
2. RBAC physically confines copies to `writeNamespaces`, so no copy can exist
   outside the set we probe.

`status` thus becomes a fast-path cache, not the sole source of truth, and the
system self-heals: a wiped status is rebuilt on the next reconcile.

### Stable copy naming

Copies are named after the owning **Whisper**, and carry its immutable UID in a
`whisperer.jeffl.es/owner-uid` label. This is deliberate, and fixes a leak that
existed independently of status:

- If copies were named after `spec.secretName` (as in the first cut), **renaming
  `secretName`** would write new copies under the new name and orphan the
  old-named ones — the orphan-delete keys on the current name and can't see the
  old copies, *with or without status*. With names tied to the (stable) Whisper, a
  rename just refreshes the same copy in place; there is no old-named orphan.
- The UID label, combined with the SSA field-manager check, is how the operator
  attributes a found copy to the right Whisper across renames, and stops one
  Whisper from reclaiming another's copy that happens to share a name.

Consequence: **the synced secret takes the Whisper's name.** Name the Whisper what
you want the replicated secret to be called (`secretName` only chooses which source
to read). This is exercised end-to-end by the `sync_and_delete_works` integration
test, which renames `secretName` and asserts the copy is refreshed in place with no
old-named orphan left behind.

**Why `get`, not `list`.** Threat-modeling the capability: the operator's
ServiceAccount token is the trust boundary; the concern is what a holder of a
leaked token can do in the (shared, multi-tenant) `writeNamespaces`.

- The token *already* holds `get`/`create`/`update`/`patch`/`delete` on secrets
  there. `get` of the deterministic copy name adds **no capability** — it's a name
  the operator already writes.
- `list` *would* add capability: enumeration. A leaked token could then dump
  **every** secret in each `writeNamespace`, including unrelated tenant secrets
  whose names it doesn't know. That meaningfully widens the blast radius.

So recovery uses `get` only, and the `secret-writer` Role grants no `list`. The
one residual: a non-whisperer secret in a `writeNamespace` that happens to share
the name is transiently `get`-read into operator memory before
[`is_owned_copy`](0001-cross-namespace-sync-authorization.md) rejects it — but
that is identical to the existing pre-delete origin check, the data is never
logged, and nothing is done with it. Net new exposure: none.

### Deployment assumptions

The security boundary rests on the cluster operator's choice of `writeNamespaces`
and `drainNamespaces`:

- **Those namespaces must not be writable by untrusted tenants.** The operator's
  copy markers (`whisper` label, owner-UID, SSA field manager) are best-effort —
  the field-manager name is a client-supplied string, so a tenant who can write
  Secrets in a target namespace can forge a look-alike. The operator mitigates
  with RBAC-confined deletes and UID/resourceVersion delete preconditions, but if
  a tenant already has Secret write in a target namespace they can tamper there
  directly regardless of whisperer. So treat the consent label as deciding *where
  copies may go*, not *who may modify them afterward*.
- The operator's own namespace (`CONTROLLER_NAMESPACE`, derived from the
  ServiceAccount) is always protected and never a target.

## Consequences

- **Status loss is survivable, not a manual-cleanup edge case.** Recovery needs
  only `get` over `writeNamespaces` — no cluster-wide read. This is machine-checked
  by the TLA+ model in [`docs/model/`](../model/): with status loss enabled, the
  `Safety`, `CleanupReclaims`, and `Convergence` properties all still hold, and the
  `scanned` (probe) term is shown to be load-bearing — remove it and TLC strands an
  orphan.
- **Sources must live in the operator's namespace** (where the scoped watch and
  read Role are). True for the cluster that motivated this. Supporting sources
  elsewhere would mean making the watched/source namespace set configurable and
  extending the read Role/RoleBindings.
- `writeNamespaces` must be a superset of every Whisper's `spec.namespaces`; a
  consenting-but-unlisted target is refused by the API server, not the operator.
  It is passed to the operator as `WRITE_NAMESPACES` for the recovery probe.
- **Decommissioning a namespace** is two-phase, so it never strands copies. Move
  the namespace from `writeNamespaces` to **`drainNamespaces`** (don't just delete
  it): the operator keeps a `get`+`delete`-only RoleBinding and keeps probing it,
  but never writes new copies there and reclaims any it finds (a drained namespace
  is excluded from targets). Once it's empty, remove it from `drainNamespaces`. The
  two lists must be **disjoint** — the chart `fail`s the render and the operator
  refuses to start (`assert`) on overlap, since a namespace can't be both an active
  target and reclaim-only. The TLA+ model proves the bound this preserves
  (`CopiesBounded`: every copy stays in `active ∪ drain`, so it's never stranded);
  `Decommission`/`Retire` are the only ways a namespace leaves the write set, and
  `Retire` is guarded on the namespace being empty.
- Pure reconcile logic (`resolve_targets`, `plan`, `parse_namespaces`) is covered
  by unit + property tests; the status-loss recovery path is covered by the
  live-cluster integration test (`sync_and_delete_works`, run via
  `scripts/integration-test.sh`).

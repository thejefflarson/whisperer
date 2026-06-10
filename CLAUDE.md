# whisperer — Kubernetes Secret Syncer

A Kubernetes operator that syncs secrets across namespaces. Sync is configured by
a namespaced `Whisper` custom resource that lives in the source namespace, names a
Secret there (`spec.secretName`), and lists target namespaces (`spec.namespaces`).
The operator watches `Whisper`s — its own API group — so it never needs
cluster-wide Secret `list`/`watch`. See [ADR 0003](docs/adr/0003-crd-scoped-rbac.md).

## Architecture

| File | Purpose |
|---|---|
| `src/whisper.rs` | The `Whisper` CustomResource (`WhisperSpec`, `WhisperStatus`) |
| `src/controller.rs` | Core reconciliation — watches `Whisper`s + operator-namespace source secrets, syncs to targets, recovers/cleans orphans (`plan`, `discover_copies`) |
| `src/bin/crdgen.rs` | Emits the `Whisper` CRD YAML (`cargo run --bin crdgen`) |
| `src/election.rs` | Leader election using Kubernetes Leases — only the elected leader runs reconciliation |
| `src/server.rs` | Health check HTTP server (`GET /ruok` → `imok`) |
| `src/metrics.rs` | OpenTelemetry metrics (reconciliations, failures, duration via RAII `Record` guard) |
| `src/ext.rs` | `SecretExt` trait with `dup()` for copying a secret into another namespace |
| `src/labels.rs` | All label/finalizer name constants |
| `src/error.rs` | Custom error enum via `thiserror` |
| `src/utils.rs` | Graceful shutdown signal handler (SIGTERM + Ctrl+C) |
| `src/telemetry.rs` | Stub — tracing is currently set up inline in `main.rs` |

A formal TLA+ model of the reconcile loop lives in [`docs/model/`](docs/model/).

## Key Labels

Config lives on the `Whisper` CR, not on Secrets. The remaining labels mark and
attribute the **copies** the operator writes:

- `whisperer.jeffl.es/allow-sync=true` — **on a target namespace**: consent to receive copies (required; the authorization boundary)
- `whisperer.jeffl.es/whisper=true` — marks managed synced copies (do not add manually)
- `whisperer.jeffl.es/owner-uid=<uid>` — on copies, the owning `Whisper`'s immutable UID (used to attribute/reclaim copies across `secretName` renames)
- `whisperer.jeffl.es/name=<name>` — on copies, the owning `Whisper`'s name (copies are named after the Whisper, not the source secret)
- `whisperer.jeffl.es/namespace=<ns>` — on copies, the source (Whisper's) namespace
- `whisperer.jeffl.es/cleanup` — finalizer on the `Whisper`, reclaims copies on deletion

## Development Commands

```bash
# Run locally (requires valid kubeconfig + the Whisper CRD installed)
cargo run

# Regenerate the CRD manifest after editing src/whisper.rs
cargo run --bin crdgen > chart/crds/whisper.yaml

# Run unit + property tests (no cluster needed)
cargo nextest run

# Run live-cluster integration tests against a throwaway k3d cluster (needs Docker)
scripts/integration-test.sh

# Model-check the TLA+ spec of the reconcile loop (needs java)
scripts/check-model.sh

# Build release binary
cargo build --release
```

## Environment Variables

| Variable | Required | Description |
|---|---|---|
| `HEALTHCHECK_PORT` | Yes | Port for the health check HTTP server |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | No | OTLP collector endpoint (default: `http://localhost:4318`) |
| `RUST_LOG` | No | Log filter, e.g. `whisperer=info` |
| `CONTROLLER_POD_NAME` | No | Pod name used in Kubernetes event reporting |
| `CONTROLLER_NAMESPACE` | No | Operator's own namespace; never a valid sync target (downward API in the chart) |
| `WRITE_NAMESPACES` | No | Comma-separated namespaces the operator may write into; name-probed to recover orphans if status is lost (chart `writeNamespaces`) |
| `DRAIN_NAMESPACES` | No | Namespaces being decommissioned: probed + reclaimed, never written to (chart `drainNamespaces`). Must be disjoint from `WRITE_NAMESPACES` — the operator panics on overlap |

The `.env` file is gitignored and loaded via `dotenvy`. See `.env` for local defaults.

## Reconciliation Logic

1. On `Apply` (create/update): read the source secret from the `Whisper`'s namespace; resolve consenting targets; determine current copy locations from `status.syncedNamespaces` **unioned with** a `get`-probe of `WRITE_NAMESPACES` (`discover_copies`); delete copies whose namespace is no longer a target; apply (idempotent SSA) to every current target; record targets in `status`. Copies are named after the `Whisper` and tagged with its UID, so a `secretName` rename refreshes them in place.
2. On `Cleanup` (finalizer): reclaim the **copies** (from status + spec + the probe), leaving the user's source secret alone.
3. Only the elected leader reconciles; followers return `Action::await_change()`.
4. The source-secret watch is scoped to the operator's own namespace; orphan recovery uses `get` over `WRITE_NAMESPACES` only — there is no cluster-wide secret `list`/`watch`.

## Leader Election

Uses raw Kubernetes `Lease` objects (not kube-rs built-in leader election). States: `Standby` → `Leading` or `Following`. Lease duration is 15s; renewal runs every 5s. On shutdown, the leader patches the lease duration to 1s to trigger fast failover.

## Testing

- Unit + property tests run with `cargo nextest run` (no cluster). Property tests (`proptest`) cover `resolve_targets` and `plan`.
- Unit tests in `election.rs` use `httpmock` to mock Kubernetes API responses.
- Integration test in `controller.rs` (`sync_and_delete_works`) is `#[ignore]` and requires a live cluster with the Whisper CRD; run it via `scripts/integration-test.sh` (spins up k3d). It exercises sync, orphan reclaim, status-loss recovery, and `secretName`-rename-in-place.
- The TLA+ model in `docs/model/` machine-checks safety/convergence/cleanup, including under status loss and renames.
- HTTP mock recordings are stored in `recordings/` (gitignored).

## CI/CD

- `rust.yml` — runs `cargo nextest` on PRs/pushes, builds and pushes a signed Docker image to GHCR on push to main. Self-hosted `whisperer-runners` (required for Docker BuildKit remote endpoint).
- `integration.yml` — runs `scripts/integration-test.sh` (k3d) on ephemeral `ubuntu-latest` (safe for fork PRs; needs no secrets).
- `helm.yaml` — packages and publishes the Helm chart to GHCR OCI on push to main.
- `dependabot-auto-merge.yml` — automatically approves and merges minor/patch Dependabot PRs after CI passes.

## Deployment

```bash
# Install via Helm (installs the Whisper CRD + operator). writeNamespaces must be
# a superset of every Whisper's spec.namespaces.
helm install whisperer oci://ghcr.io/thejefflarson/charts/whisperer \
  --set 'writeNamespaces={ns1,ns2}'
```

Key Helm values: `image.repository`, `image.tag`, `otelEndpoint`, `writeNamespaces`.

## Security Notes

- Secret data is never logged.
- Config lives on a `Whisper` CR in the **source** namespace — you can only whisper a secret out of a namespace where you can already create objects.
- The operator has **no cluster-wide Secret access**: it reads sources only in its own namespace and writes only into `writeNamespaces` (a RoleBinding each). Orphan recovery uses `get`, never `list`. See [ADR 0003](docs/adr/0003-crd-scoped-rbac.md).
- Decommission a target via `drainNamespaces` (get+delete RoleBinding, reclaim-only) rather than deleting it from `writeNamespaces`, which would strand its copies. The two lists must be disjoint (chart `fail` + operator `assert`).
- Target namespaces must consent with `whisperer.jeffl.es/allow-sync=true`; protected namespaces (`kube-system`, `kube-public`, `kube-node-lease`, the operator's own `CONTROLLER_NAMESPACE`) are never targets. See [ADR 0001](docs/adr/0001-cross-namespace-sync-authorization.md).
- Cross-namespace deletes require the operator's copy markers (`whisper` + field manager + owner-UID) AND carry the verified object's UID/resourceVersion as delete preconditions. The markers are best-effort (the SSA field-manager name is client-supplied, not an unforgeable identity); the real boundary is RBAC confining deletes to `writeNamespaces`/`drainNamespaces`, which must not be tenant-writable. See [ADR 0001](docs/adr/0001-cross-namespace-sync-authorization.md)/[0003](docs/adr/0003-crd-scoped-rbac.md).
- Copies are named after the owning `Whisper` (stable across `secretName` renames) and carry its UID, so a rename refreshes in place instead of orphaning the old copy.
- The health check server binds to `0.0.0.0` — required for Kubernetes liveness/readiness probes.
- The runtime image runs as non-root UID 65532; the chart sets a hardened `securityContext`.
- Images and Helm charts are signed with cosign.

## Design Decisions

Durable design decisions are recorded as ADRs in [`docs/adr/`](docs/adr/):

- [ADR 0001 — Cross-namespace sync authorization](docs/adr/0001-cross-namespace-sync-authorization.md)
- [ADR 0002 — Security review remediation (v0.2.0)](docs/adr/0002-security-review-remediation.md)
- [ADR 0003 — Whisper CRD and scoped RBAC](docs/adr/0003-crd-scoped-rbac.md)

Prefer recording a new decision as an ADR (or a note in this file) over leaving it implicit, so it is version-controlled and reviewable.

## Known Issues / TODOs

- _None currently. (`telemetry.rs` now owns the OTLP pipelines and sets `service.name`; the runtime image is `debian:bookworm-slim` running as non-root UID 65532.)_

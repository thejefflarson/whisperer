# whisperer — Kubernetes Secret Syncer

A Kubernetes operator that syncs secrets across namespaces. Secrets opt in via a label and specify target namespaces via an annotation.

## Architecture

| File | Purpose |
|---|---|
| `src/controller.rs` | Core reconciliation logic — watches source secrets, syncs to target namespaces, handles orphan cleanup |
| `src/election.rs` | Leader election using Kubernetes Leases — only the elected leader runs reconciliation |
| `src/server.rs` | Health check HTTP server (`GET /ruok` → `imok`) |
| `src/metrics.rs` | OpenTelemetry metrics (reconciliations, failures, duration via RAII `Record` guard) |
| `src/ext.rs` | `SecretExt` trait with `dup()` for copying a secret into another namespace |
| `src/labels.rs` | All label/annotation/finalizer name constants |
| `src/error.rs` | Custom error enum via `thiserror` |
| `src/utils.rs` | Graceful shutdown signal handler (SIGTERM + Ctrl+C) |
| `src/telemetry.rs` | Stub — tracing is currently set up inline in `main.rs` |

## Key Labels and Annotations

- `whisperer.jeffl.es/sync=true` — marks a secret as a sync source (required)
- `whisperer.jeffl.es/namespaces=ns1,ns2` — comma-separated target namespaces (required on source)
- `whisperer.jeffl.es/whisper=true` — marks managed synced copies (do not add manually)
- `whisperer.jeffl.es/name=<name>` — on copies, identifies source secret name
- `whisperer.jeffl.es/namespace=<ns>` — on copies, identifies source namespace
- `whisperer.jeffl.es/cleanup` — finalizer ensuring cleanup on source deletion

## Development Commands

```bash
# Run locally (requires valid kubeconfig)
cargo run

# Run tests (excludes live-cluster integration tests)
cargo nextest run

# Run all tests including integration tests (requires a running cluster)
cargo nextest run --include-ignored

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

The `.env` file is gitignored and loaded via `dotenvy`. See `.env` for local defaults.

## Reconciliation Logic

1. On `Apply` (create/update): list existing synced copies, delete any whose namespace is no longer in the annotation, then apply (idempotent patch) to all current target namespaces.
2. On `Cleanup` (finalizer): delete the source secret, all annotated target copies, and any orphaned copies found by label selector.
3. Only the elected leader reconciles; followers return `Action::await_change()`.

## Leader Election

Uses raw Kubernetes `Lease` objects (not kube-rs built-in leader election). States: `Standby` → `Leading` or `Following`. Lease duration is 15s; renewal runs every 5s. On shutdown, the leader patches the lease duration to 1s to trigger fast failover.

## Testing

- Unit tests in `election.rs` use `httpmock` to mock Kubernetes API responses.
- Integration test in `controller.rs` (`sync_and_delete_works`) is `#[ignore]` and requires a live cluster.
- HTTP mock recordings are stored in `recordings/` (gitignored).

## CI/CD

- `rust.yml` — runs `cargo nextest` on PRs/pushes, builds and pushes a signed Docker image to GHCR on push to main.
- `helm.yaml` — packages and publishes the Helm chart to GHCR OCI on push to main.
- `dependabot-auto-merge.yml` — automatically approves and merges minor/patch Dependabot PRs after CI passes.
- Runs on self-hosted `whisperer-runners` (required for Docker BuildKit remote endpoint).

## Deployment

```bash
# Install via Helm
helm install whisperer oci://ghcr.io/thejefflarson/charts/whisperer
```

Key Helm values: `image.repository`, `image.tag`, `otelEndpoint`.

## Security Notes

- Secret data is never logged.
- Syncing requires explicit opt-in (`whisperer.jeffl.es/sync=true`).
- Synced copies strip the source finalizer and namespace annotation to prevent unintended re-sync.
- The health check server binds to `0.0.0.0` — required for Kubernetes liveness/readiness probes.
- Images and Helm charts are signed with cosign.

## Known Issues / TODOs

- `telemetry.rs` is a stub; tracing setup lives in `main.rs` directly.
- The runtime Docker image uses `rust:latest` (large). Consider `debian:bookworm-slim` or distroless.

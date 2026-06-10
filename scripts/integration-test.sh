#!/usr/bin/env bash
# Run whisperer's live-cluster integration tests against a throwaway k3d cluster.
#
# The #[ignore]d tests in src/controller.rs (e.g. sync_and_delete_works) drive
# the controller's apply()/cleanup() directly against a real apiserver, so they
# need a cluster with the Whisper CRD installed. This spins one up with k3d,
# installs the generated CRD, runs the tests, and tears the cluster down again.
#
# Requires: docker (running), k3d, kubectl, and the Rust toolchain.
# Usage:   scripts/integration-test.sh [--keep]
#   --keep   leave the cluster running afterwards (for debugging)
set -euo pipefail

CLUSTER="${CLUSTER:-whisperer-it}"
KEEP=0
[[ "${1:-}" == "--keep" ]] && KEEP=1

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

for tool in docker k3d kubectl cargo; do
  command -v "$tool" >/dev/null 2>&1 || { echo "error: '$tool' not found on PATH" >&2; exit 1; }
done
docker info >/dev/null 2>&1 || { echo "error: docker daemon is not running" >&2; exit 1; }

cleanup() {
  if [[ "$KEEP" -eq 0 ]]; then
    echo "==> deleting cluster '$CLUSTER'"
    k3d cluster delete "$CLUSTER" >/dev/null 2>&1 || true
  else
    echo "==> leaving cluster '$CLUSTER' running (--keep); delete with: k3d cluster delete $CLUSTER"
  fi
}
trap cleanup EXIT

echo "==> creating k3d cluster '$CLUSTER'"
k3d cluster create "$CLUSTER" --wait --no-lb --k3s-arg "--disable=traefik@server:0"

# Point kubectl/kube-rs at the new cluster for the rest of this script. Set a
# single path (k3d warns if KUBECONFIG already lists several) so neither kubectl
# nor kube-rs touches the developer's real clusters.
unset KUBECONFIG
export KUBECONFIG
KUBECONFIG="$(k3d kubeconfig write "$CLUSTER")"

echo "==> installing the Whisper CRD"
cargo run --quiet --bin crdgen | kubectl apply -f -
kubectl wait --for=condition=Established crd/whispers.whisperer.jeffl.es --timeout=60s

echo "==> running integration tests"
# The k3d apiserver is local. kube-rs (built without the `http-proxy` feature)
# refuses to start when HTTP(S)_PROXY is set, even for a localhost target, so
# clear any proxy for the test process.
unset HTTP_PROXY HTTPS_PROXY ALL_PROXY http_proxy https_proxy all_proxy
# Run the #[ignore]d live-cluster tests (plain cargo test — no extra tooling), on
# one thread so the shared namespaces the tests create don't collide.
cargo test -- --ignored --test-threads=1

echo "==> integration tests passed"

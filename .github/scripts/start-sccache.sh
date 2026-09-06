#!/usr/bin/env bash
# Fail-soft sccache backend selection for the CI Rust job. The backend is R2
# (cluster repo ADR-0020); this script is what makes losing it soft.
#
# The runner pod injects the R2 config -- SCCACHE_BUCKET / SCCACHE_ENDPOINT /
# SCCACHE_REGION / SCCACHE_S3_KEY_PREFIX / SCCACHE_S3_USE_SSL plus the bucket-scoped
# AWS_* token -- from the `sccache-r2` Secret via envFrom (cluster repo:
# charts/actions/runners/values.yaml). Nothing about the backend is set in the
# workflow, and no credential is committed here.
#
# WHY THIS SCRIPT STILL EXISTS AFTER THE R2 MOVE. sccache's S3 backend is EAGER, exactly
# like the redis one it replaced: `sccache --start-server` FAILS outright when the bucket
# is unreachable, and the first rustc-through-sccache call then dies with "sccache: Timed
# out waiting for server startup", killing the whole job. Measured against sccache 0.16.0:
#
#   S3 configured, endpoint unreachable    -> --start-server FAILS
#   empty SCCACHE_BUCKET + SCCACHE_DIR set -> "Cache location: Local disk"
#
# So "R2 down means cache misses and slower builds" only holds where something degrades on
# purpose. That something is this script. Without it, an R2 outage is a hard CI outage
# rather than a slow one.
#
# On success: leave the pod's S3 env alone and exit 0.
# On failure: fall back to a LOCAL disk cache. GITHUB_ENV cannot *unset* a variable, so
# the fallback exports an EMPTY SCCACHE_BUCKET (which sccache treats as unconfigured --
# the measurement above) alongside SCCACHE_DIR. Exporting SCCACHE_DIR alone would not
# work: a non-empty SCCACHE_BUCKET still wins.
set -uo pipefail

fall_back_to_local_disk() {
  local local_dir="${HOME}/.cache/sccache"
  {
    echo "SCCACHE_BUCKET="
    echo "SCCACHE_DIR=${local_dir}"
  } >> "${GITHUB_ENV}"
  sccache --stop-server >/dev/null 2>&1 || true
  SCCACHE_BUCKET= SCCACHE_DIR="${local_dir}" sccache --start-server >/dev/null 2>&1 || true
}

# Start clean -- no server should be running yet (no earlier step compiles), but make the
# backend switch deterministic if one lingered.
sccache --stop-server >/dev/null 2>&1 || true

if [ -z "${SCCACHE_BUCKET:-}" ]; then
  # The `sccache-r2` Secret never reached the pod (Secret missing, or an overlay that
  # re-enumerates the runner container dropped the envFrom). Say so loudly:
  # silently compiling against a cold local cache is how a broken cutover reads as fine.
  echo "::warning::sccache: no R2 config in the pod env (sccache-r2 Secret missing?) — using a local disk cache"
  fall_back_to_local_disk
  exit 0
fi

for attempt in 1 2 3; do
  if sccache --start-server >/dev/null 2>&1; then
    echo "sccache: R2 backend up (bucket=${SCCACHE_BUCKET}, attempt ${attempt})"
    exit 0
  fi
  echo "sccache: R2 backend not ready (attempt ${attempt}/3); retrying in 3s" >&2
  sccache --stop-server >/dev/null 2>&1 || true
  sleep 3
done

echo "::warning::sccache: R2 backend unreachable — falling back to local disk cache"
fall_back_to_local_disk

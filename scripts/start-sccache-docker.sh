#!/bin/sh
# Fail-soft sccache backend selection INSIDE the Docker image build.
# The in-build twin of .github/scripts/start-sccache.sh — same shape, different
# transport for the config: the CI script reads the runner pod's env directly,
# this one reads BuildKit build secrets mounted at /run/secrets by the RUN that
# calls it.
#
# Usage:
#   sh scripts/start-sccache-docker.sh              # start a server, degrade R2 to local disk on demand
#   sh scripts/start-sccache-docker.sh --selftest   # run the built-in fixtures
#
# Exit status: 0 once a cache server (R2 or local disk) is actually listening.
# Degrading R2 -> local disk is soft, per the rationale below, and never fails
# the build. But if even the local-disk fallback can't bind, that is NOT another
# soft case: on this shared BuildKit netns it almost always means
# SCCACHE_SERVER_PORT collided with a concurrent build (see the Dockerfile
# comment on that variable), and a swallowed failure here would let cargo
# silently attach to that OTHER build's sccache server — and its R2 credentials
# — instead of a local one. So that path is the one case this script fails on.
#
# WHY SECRETS AND NOT ENV/BUILD-ARGS. `ENV AWS_SECRET_ACCESS_KEY=…` or an ARG
# consumed in a layer persists in `docker history` for every image we push to
# ghcr — that would publish the R2 token. A build secret is mounted for exactly
# one RUN, lands in no layer, and (unlike a build-arg) is not part of the layer
# cache key, so rotating the R2 config never churns the registry layer cache.
#
# WHY THIS IS FAIL-SOFT, WHERE THE REDIS PREDECESSOR WAS A HARD GATE. That
# backend was an in-cluster Service reachable with no credentials, so "can't
# reach it" really did mean "this builder is misconfigured" and failing the
# build was the honest signal. R2 is a remote bucket behind a rotatable token: a
# transient blip, an expired key or a missing `sccache-r2` Secret would now fail
# a build for a reason that has nothing to do with the code. On a release tag
# that costs a VERSION NUMBER (published tags are immutable — a failed release
# burns v0.X.Y), where degrading costs one cold compile, i.e. exactly what a
# cache miss already costs. So: probe, retry, degrade.
#
# sccache's S3 backend is EAGER, exactly like the redis one it replaced:
# `sccache --start-server` FAILS outright when the bucket is unreachable, and
# the first rustc-through-sccache call then dies with "sccache: Timed out
# waiting for server startup". Measured against sccache 0.16.0:
#
#   S3 configured, endpoint unreachable    -> --start-server FAILS
#   empty SCCACHE_BUCKET + SCCACHE_DIR set -> "Cache location: Local disk"
#
# Only the sccache SERVER holds the backend config, so this script exporting
# nothing back to its caller is fine: the later `cargo build` and
# `sccache --show-stats` just talk to whichever server we left running on
# SCCACHE_SERVER_PORT (exported by the caller before invoking us).
set -u

# Overridable only so --selftest can point at fixture files; the Dockerfile
# never sets it, and BuildKit always mounts secrets under /run/secrets.
SECRET_DIR="${SCCACHE_SECRET_DIR:-/run/secrets}"

# Non-empty contents of a mounted build secret, or nothing. `required=false` is
# the Dockerfile default, so an unprovided secret is simply an absent/empty file
# (local `docker build` / docker-compose, which pass no secrets at all).
secret() {
    _f="${SECRET_DIR}/$1"
    [ -s "$_f" ] || return 0
    tr -d '\r\n' <"$_f"
}

# Local disk lives under the /app/target BuildKit cache mount so a degraded
# build still warms something for the next degraded build. GITHUB_ENV has no
# equivalent here — we just start the server ourselves with the fallback
# config. An empty SCCACHE_BUCKET reads as "unconfigured"; SCCACHE_DIR alone is
# not enough, a non-empty bucket still wins.
fall_back_to_local_disk() {
    sccache --stop-server >/dev/null 2>&1 || true
    if env -u AWS_ACCESS_KEY_ID -u AWS_SECRET_ACCESS_KEY \
        SCCACHE_BUCKET= SCCACHE_DIR=/app/target/.sccache-local \
        sccache --start-server >/dev/null 2>&1; then
        return 0
    fi
    # Deliberately NOT `|| true`'d away: a bind failure here on a fixed
    # SCCACHE_SERVER_PORT almost always means a concurrent build on this same
    # BuildKit daemon already owns that port, so cargo would otherwise silently
    # talk to that build's sccache server instead of one of its own.
    echo "sccache: local disk cache failed to start (possible SCCACHE_SERVER_PORT collision on this BuildKit daemon)" >&2
    return 1
}

start_sccache() {
    # Make the backend switch deterministic if an earlier client call spawned one.
    sccache --stop-server >/dev/null 2>&1 || true

    SCCACHE_BUCKET=$(secret SCCACHE_BUCKET)
    if [ -z "${SCCACHE_BUCKET}" ]; then
        echo "sccache: no R2 config in the build secrets — using a local disk cache" >&2
        fall_back_to_local_disk
        return $?
    fi

    AWS_ACCESS_KEY_ID=$(secret AWS_ACCESS_KEY_ID)
    AWS_SECRET_ACCESS_KEY=$(secret AWS_SECRET_ACCESS_KEY)
    SCCACHE_ENDPOINT=$(secret SCCACHE_ENDPOINT)
    SCCACHE_REGION=$(secret SCCACHE_REGION)
    SCCACHE_S3_KEY_PREFIX=$(secret SCCACHE_S3_KEY_PREFIX)
    SCCACHE_S3_USE_SSL=$(secret SCCACHE_S3_USE_SSL)
    export SCCACHE_BUCKET AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY \
        SCCACHE_ENDPOINT SCCACHE_REGION SCCACHE_S3_KEY_PREFIX SCCACHE_S3_USE_SSL

    for attempt in 1 2 3; do
        # The 10s guard bounds a genuinely-unreachable bucket (a normal start is
        # ~0.4s); the retries ride out a transient R2 blip.
        if timeout 10 sccache --start-server >/dev/null 2>&1; then
            echo "sccache: R2 backend up (bucket=${SCCACHE_BUCKET}, attempt ${attempt})" >&2
            return 0
        fi
        echo "sccache: R2 backend not ready (attempt ${attempt}/3); retrying in 3s" >&2
        sccache --stop-server >/dev/null 2>&1 || true
        sleep 3
    done

    echo "sccache: R2 backend unreachable — falling back to local disk cache" >&2
    fall_back_to_local_disk
    return $?
}

# ── Self-test ────────────────────────────────────────────────────────────────
# Drives the three states a build can be in against stub `sccache`/`timeout`
# binaries, so a regression in the fail-soft logic is caught by CI in seconds
# instead of by a burned release tag. The stub records the backend env of every
# `--start-server` it is asked to run.
selftest() {
    _tmp="$(mktemp -d)"
    trap 'rm -rf "${_tmp}"' EXIT
    mkdir -p "${_tmp}/bin" "${_tmp}/secrets"

    # STUB_START_FAILS fails an attempt with SCCACHE_BUCKET set (the R2/S3
    # backend); STUB_LOCAL_FAILS fails one with it empty (the local-disk
    # fallback that fall_back_to_local_disk starts) — modeling the two failures
    # separately since a real bucket-unreachable and a real port collision are
    # independent events.
    cat >"${_tmp}/bin/sccache" <<'STUB'
#!/bin/sh
if [ "${1:-}" = "--start-server" ]; then
  echo "start bucket=[${SCCACHE_BUCKET:-}] dir=[${SCCACHE_DIR:-}] key=[${AWS_ACCESS_KEY_ID:-}]" \
    >>"${STUB_LOG}"
  if [ -n "${SCCACHE_BUCKET:-}" ]; then
    [ "${STUB_START_FAILS:-0}" = "1" ] && exit 1
  else
    [ "${STUB_LOCAL_FAILS:-0}" = "1" ] && exit 1
  fi
fi
exit 0
STUB
    # `timeout N cmd …` — drop the duration, run the command. Shadows the real
    # coreutils binary so the fixtures behave the same on Linux and macOS.
    cat >"${_tmp}/bin/timeout" <<'STUB'
#!/bin/sh
shift
exec "$@"
STUB
    chmod +x "${_tmp}/bin/sccache" "${_tmp}/bin/timeout"
    PATH="${_tmp}/bin:${PATH}"
    export PATH STUB_LOG
    SECRET_DIR="${_tmp}/secrets"

    _fails=0
    expect() { # expect <label> <grep-pattern>
        if grep -q "$2" "${STUB_LOG}"; then
            echo "  ok   $1"
        else
            echo "  FAIL $1 — no line matching /$2/ in:" >&2
            sed 's/^/       /' "${STUB_LOG}" >&2
            _fails=$((_fails + 1))
        fi
    }

    # 1. No secrets at all (plain `docker build`, docker-compose): local disk,
    #    and NOT a hard failure.
    STUB_LOG="${_tmp}/log1"; : >"${STUB_LOG}"
    STUB_START_FAILS=0 start_sccache >/dev/null 2>&1 || _fails=$((_fails + 1))
    expect "no config -> local disk" 'bucket=\[\] dir=\[/app/target/.sccache-local\]'

    # 2. Config present and the bucket answers: S3 backend, credential passed
    #    through, no fallback.
    printf 'a-bucket' >"${_tmp}/secrets/SCCACHE_BUCKET"
    printf 'a-key-id' >"${_tmp}/secrets/AWS_ACCESS_KEY_ID"
    STUB_LOG="${_tmp}/log2"; : >"${STUB_LOG}"
    STUB_START_FAILS=0 start_sccache >/dev/null 2>&1 || _fails=$((_fails + 1))
    expect "R2 reachable -> S3 backend" 'bucket=\[a-bucket\] dir=\[\] key=\[a-key-id\]'
    if [ "$(grep -c 'start ' "${STUB_LOG}")" != "1" ]; then
        echo "  FAIL R2 reachable -> exactly one start (no fallback)" >&2
        _fails=$((_fails + 1))
    else
        echo "  ok   R2 reachable -> exactly one start (no fallback)"
    fi

    # 3. Config present, bucket unreachable: retried, then degraded to local
    #    disk WITHOUT the credential — and still exit 0. This is the case that
    #    used to fail the build (and a release tag with it).
    STUB_LOG="${_tmp}/log3"; : >"${STUB_LOG}"
    STUB_START_FAILS=1 start_sccache >/dev/null 2>&1 || _fails=$((_fails + 1))
    expect "R2 down -> local disk fallback" 'bucket=\[\] dir=\[/app/target/.sccache-local\] key=\[\]'
    if [ "$(grep -c 'bucket=\[a-bucket\]' "${STUB_LOG}")" != "3" ]; then
        echo "  FAIL R2 down -> 3 attempts before degrading" >&2
        _fails=$((_fails + 1))
    else
        echo "  ok   R2 down -> 3 attempts before degrading"
    fi

    # 4. No R2 config AND the local-disk fallback itself fails to bind — the
    #    SCCACHE_SERVER_PORT-collision case (two builds on the same BuildKit
    #    daemon picking the same port). This must be a hard failure: silently
    #    continuing would leave cargo talking to some OTHER build's sccache
    #    server. Regression test for the bug this replaces (`|| true` used to
    #    swallow exactly this).
    rm -f "${_tmp}/secrets/SCCACHE_BUCKET" "${_tmp}/secrets/AWS_ACCESS_KEY_ID"
    STUB_LOG="${_tmp}/log4"; : >"${STUB_LOG}"
    if STUB_LOCAL_FAILS=1 start_sccache >/dev/null 2>&1; then
        echo "  FAIL local-disk bind collision -> must exit nonzero, exited 0" >&2
        _fails=$((_fails + 1))
    else
        echo "  ok   local-disk bind collision -> exits nonzero (surfaced, not swallowed)"
    fi

    if [ "${_fails}" -ne 0 ]; then
        echo "selftest: ${_fails} failure(s)" >&2
        return 1
    fi
    echo "selftest: all fixtures passed"
}

if [ "${1:-}" = "--selftest" ]; then
    selftest
else
    start_sccache
fi

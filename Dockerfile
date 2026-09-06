# Builder must match the runtime's glibc. debian:bookworm-slim ships glibc 2.36;
# the default cargo-chef:latest-rust-1 is a newer Debian (glibc 2.41), which
# produces a binary that fails on bookworm with "GLIBC_2.39 not found". The
# -bookworm tag is built on bookworm (glibc 2.36) so the two stay in sync. We
# keep this image for its bookworm rust toolchain even though cargo-chef itself
# is no longer used (see below).
FROM lukemathwalker/cargo-chef:latest-rust-1-bookworm AS builder
WORKDIR /app

# sccache — the dep-caching layer, backed by the shared Cloudflare R2 bucket
# (cluster repo: charts/sccache, ADR-0020), so a workspace dep compiled by any
# repo's CI (or image build) is reused here. cargo-chef was removed: sccache
# and cargo-chef can't coexist for Rust — sccache keys its cache on a hash of
# every `--extern` input, and cargo always passes `.rmeta` metadata externs, but
# cargo-chef's `cook` leaves the shared target dir in a state where those extern
# `.rmeta` files don't survive into the final `cargo build`, so sccache fatals
# ("Failed to open file for hashing: …/lib*.rmeta: No such file or directory")
# and aborts (JEF-389, confirmed). A single builder stage sidesteps that. The
# BuildKit cache mounts below still persist the cargo registry/git + compiled
# target dir across builds on the shared in-cluster BuildKit daemon.
# glibc (-gnu) build to match the bookworm toolchain (not musl).
#
# The release tarball is downloaded to a file and checked against its published
# sha256 (sccache ships a `.sha256` per release asset) before extraction, instead
# of piping wget straight into tar. This was HTTPS + version-pinned already, so
# it's hardening rather than a fix for an active hole — but an unverified archive
# was being extracted inside the very RUN that mounts seven BuildKit secrets.
# Bump the digest below whenever `ver` bumps.
RUN set -eux; ver=0.16.0; \
    case "$(uname -m)" in \
      x86_64)  a=x86_64;  sha=aec995a83ad3dff3d14b6314e08858b7b73d35ca85a5bcf3d3a9ec07dee35588 ;; \
      aarch64) a=aarch64; sha=f73a5c39f96bb6ebb89cc7915cf182260d4cbf30765322c5e793d0fe8bd80784 ;; \
      *) echo "unsupported arch $(uname -m)" >&2; exit 1 ;; \
    esac; \
    f="sccache-v${ver}-${a}-unknown-linux-musl.tar.gz"; \
    wget -qO "/tmp/${f}" "https://github.com/mozilla/sccache/releases/download/v${ver}/${f}"; \
    echo "${sha}  /tmp/${f}" | sha256sum -c -; \
    tar -xz -C /usr/local/bin --strip-components=1 -f "/tmp/${f}" "sccache-v${ver}-${a}-unknown-linux-musl/sccache"; \
    rm -f "/tmp/${f}"
ENV RUSTC_WRAPPER=sccache CARGO_INCREMENTAL=0

# Build application. The R2 config + its bucket-scoped AWS_* token arrive as BuildKit
# BUILD SECRETS (rust.yml's `secret-envs`, fed from the runner pod's `sccache-r2`
# envFrom) — never ENV or a build-arg, both of which persist in `docker history` on
# every image we push. scripts/start-sccache-docker.sh probes R2 and degrades to a
# local disk cache if it can't be reached: sccache's S3 backend is EAGER (unlike the
# in-cluster redis it replaced, an unreachable bucket FAILS `--start-server` outright),
# and a hard gate here would fail a release on an R2 blip or an expired token — costing
# an immutable version tag for what a cache miss already costs. A unique random
# SCCACHE_SERVER_PORT is required because BuildKit build sandboxes share a netns, so
# concurrent builds would collide on the fixed default port 4226 ("Address in use").
# The seed comes from /dev/urandom, not awk's clock-based default: srand() with no
# argument seeds from the wall clock in whole SECONDS, so two builds starting in the
# same second on the same BuildKit daemon would pick the identical port anyway — the
# loser's `--start-server` fails "Address in use", and without a real seed that failure
# would be silently swallowed, leaving cargo to attach to the OTHER build's already-running
# sccache server (a different repo's cache instance, with that build's R2 credentials).
# The ephemeral target mount means the binary is cp'd out to /app for the final stage,
# in the same RUN so the mount is still present.
COPY . .
RUN --mount=type=cache,target=/app/target,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=secret,id=AWS_ACCESS_KEY_ID \
    --mount=type=secret,id=AWS_SECRET_ACCESS_KEY \
    --mount=type=secret,id=SCCACHE_BUCKET \
    --mount=type=secret,id=SCCACHE_ENDPOINT \
    --mount=type=secret,id=SCCACHE_REGION \
    --mount=type=secret,id=SCCACHE_S3_KEY_PREFIX \
    --mount=type=secret,id=SCCACHE_S3_USE_SSL \
    set -e; \
    export SCCACHE_SERVER_PORT=$(awk -v seed="$(od -An -N4 -tu4 /dev/urandom | tr -d '[:space:]')" \
      'BEGIN{srand(seed); print int(20000+rand()*40000)}'); \
    sh scripts/start-sccache-docker.sh; \
    cargo build --release; \
    cp /app/target/release/whisperer ./whisperer; \
    sccache --show-stats

# Slim runtime instead of the full rust image. Create a fixed-UID non-root user
# (65532) so it matches the chart's securityContext.runAsUser/runAsGroup and the
# pod can satisfy runAsNonRoot. ca-certificates is needed for TLS to the API
# server and the OTLP endpoint. The binary is dynamically linked against glibc,
# which the cargo-chef builder and bookworm-slim both provide.
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 65532 --user-group --no-create-home --shell /usr/sbin/nologin nonroot
COPY --from=builder --chown=65532:65532 /app/whisperer /app/whisperer
USER 65532:65532
HEALTHCHECK NONE
EXPOSE 8080
ENTRYPOINT ["/app/whisperer"]

# Builder must match the runtime's glibc. debian:bookworm-slim ships glibc 2.36;
# the default cargo-chef:latest-rust-1 is a newer Debian (glibc 2.41), which
# produces a binary that fails on bookworm with "GLIBC_2.39 not found". The
# -bookworm tag is built on bookworm (glibc 2.36) so the two stay in sync. We
# keep this image for its bookworm rust toolchain even though cargo-chef itself
# is no longer used (see below).
FROM lukemathwalker/cargo-chef:latest-rust-1-bookworm AS builder
WORKDIR /app

# sccache (shared in-cluster Redis rustc cache). cargo-chef was removed: sccache
# and cargo-chef can't coexist for Rust — sccache keys its cache on a hash of
# every `--extern` input, and cargo always passes `.rmeta` metadata externs, but
# cargo-chef's `cook` leaves the shared target dir in a state where those extern
# `.rmeta` files don't survive into the final `cargo build`, so sccache fatals
# ("Failed to open file for hashing: …/lib*.rmeta: No such file or directory")
# and aborts (JEF-389, confirmed). A single builder stage sidesteps that. The
# BuildKit cache mounts below still persist the cargo registry/git + compiled
# target dir across builds on the shared in-cluster BuildKit daemon.
# glibc (-gnu) build to match the bookworm toolchain (not musl).
RUN set -eux; ver=0.16.0; \
    case "$(uname -m)" in x86_64) a=x86_64 ;; aarch64) a=aarch64 ;; *) echo "unsupported arch $(uname -m)" >&2; exit 1 ;; esac; \
    wget -qO- "https://github.com/mozilla/sccache/releases/download/v${ver}/sccache-v${ver}-${a}-unknown-linux-gnu.tar.gz" \
      | tar -xz -C /usr/local/bin --strip-components=1 "sccache-v${ver}-${a}-unknown-linux-gnu/sccache"
ENV RUSTC_WRAPPER=sccache CARGO_INCREMENTAL=0 \
    SCCACHE_REDIS=redis://sccache-redis.dev.svc.cluster.local:6379

# Build application. sccache is a hard gate (no local fallback): a unique random
# SCCACHE_SERVER_PORT is required because BuildKit build sandboxes share a netns,
# so concurrent builds would collide on the fixed default port 4226
# ("Address in use"). The ephemeral target mount means the binary is cp'd out to
# /app for the final stage, in the same RUN so the mount is still present.
COPY . .
RUN --mount=type=cache,target=/app/target,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/usr/local/cargo/registry \
    set -e; \
    export SCCACHE_SERVER_PORT=$(awk 'BEGIN{srand(); print int(20000+rand()*40000)}'); \
    timeout 10 sccache --start-server; \
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

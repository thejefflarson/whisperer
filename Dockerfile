FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN --mount=type=cache,target=/app/target,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/usr/local/cargo/registry \
    cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Build dependencies - this is the caching Docker layer!
RUN --mount=type=cache,target=/app/target,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/usr/local/cargo/registry \
    cargo chef cook --release --recipe-path recipe.json

# Build application
COPY . .
RUN --mount=type=cache,target=/app/target,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release

RUN --mount=type=cache,target=/app/target,sharing=locked \
    cp /app/target/release/whisperer ./whisperer

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

# syntax=docker/dockerfile:1.7

# ---- build ----
FROM rust:1-slim-bookworm AS build

WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates pkg-config \
  && rm -rf /var/lib/apt/lists/*

# ---- dependency layer -------------------------------------------------------
# Compile only the dependency tree against a stub crate. Keyed on Cargo.toml /
# Cargo.lock alone, so a source-only change (the common case) doesn't
# recompile every dependency — mirrors ogc-dggs-api's Dockerfile, minus the
# GDAL/DGGRID base image and private-git secret mount: every dependency here
# is a public crates.io crate (reqwest's rustls-tls feature also means no
# libssl-dev/OpenSSL at build or run time either).
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs
RUN cargo build --release

# ---- application layer ------------------------------------------------------
# Real sources. Clean the stub's own crate artifacts so the real binary is
# always rebuilt from actual code, independent of source-file mtimes, while
# every dependency stays cached from the layer above. `cargo clean -p` knows
# where the artifacts are; deleting them by path broke when cargo 1.100 moved
# them, and the image then shipped the empty stub. The size check fails the
# build if that ever happens again (the stub is under 1 MB).
COPY src src
RUN cargo clean --release -p ogc-dggs-mcp && \
    cargo build --release && \
    test "$(stat -c %s target/release/ogc-dggs-mcp)" -gt 1000000

# ---- runtime ----
FROM ubuntu:24.04
# curl isn't used by the binary itself — it's here so this same image can
# probe its own /healthz and auth-check endpoints, the way ogc-dggs-api's
# runtime image includes curl for its own e2e job's before_script/health
# checks. .gitlab-ci.yml's test:e2e:smoke job runs THIS image as the job
# container and curls it from inside.
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl \
  && rm -rf /var/lib/apt/lists/*

RUN useradd -m -o -u 1000 app

COPY --from=build /app/target/release/ogc-dggs-mcp /usr/local/bin/ogc-dggs-mcp

ARG GIT_SHA=unknown
ARG GIT_REF=unknown
ENV OGC_DGGS_MCP_GIT_SHA=$GIT_SHA
ENV OGC_DGGS_MCP_GIT_REF=$GIT_REF

ENV RUST_LOG=info
# Not baked as MCP_TRANSPORT=http here: k8s sets it via env, keeping the image
# itself transport-agnostic (the same image can run stdio in a debug pod too).

EXPOSE 8080
USER app
ENTRYPOINT ["/usr/local/bin/ogc-dggs-mcp"]

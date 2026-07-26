# The image builds the binaries and copies them in — there is no second build
# path to keep in sync with the standalone binaries.

FROM rust:1.95-bookworm AS build

# protoc for the generated gRPC bindings; git because the control plane clones
# repositories to build from them.
RUN apt-get update \
    && apt-get install --no-install-recommends -y protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Manifests first, so a change to sources does not invalidate the dependency
# layer. The dummy sources exist only to make `cargo fetch` resolvable.
COPY Cargo.toml Cargo.lock ./
COPY crates/proto/Cargo.toml     crates/proto/
COPY crates/server/Cargo.toml    crates/server/
COPY crates/web/Cargo.toml       crates/web/
COPY crates/cli/Cargo.toml       crates/cli/
COPY crates/mcp/Cargo.toml       crates/mcp/
COPY crates/allinone/Cargo.toml  crates/allinone/
RUN mkdir -p crates/proto/src crates/server/src crates/web/src crates/cli/src \
             crates/mcp/src crates/allinone/src \
    && echo 'fn main() {}' | tee crates/server/src/main.rs \
         crates/web/src/main.rs crates/cli/src/main.rs \
         crates/mcp/src/main.rs crates/allinone/src/main.rs > /dev/null \
    && touch crates/proto/src/lib.rs crates/server/src/lib.rs \
             crates/web/src/lib.rs crates/mcp/src/lib.rs \
    && cargo fetch --locked

# The real sources.
COPY controlplane.proto ./
COPY crates crates

# `--offline` proves the dependency layer above was complete.
RUN cargo build --release --locked --offline \
      --bin nudo-server --bin nudo-web --bin nudo --bin nudo-mcp \
      --bin nudo-all-in-one

# ---------------------------------------------------------------------------

FROM debian:bookworm-slim

# The control plane reaches targets over SSH and clones with git; ca-certificates
# is needed to talk to GitHub. Nothing else — the deployed binaries run on the
# targets, not here.
RUN apt-get update \
    && apt-get install --no-install-recommends -y \
         ca-certificates \
         openssh-client \
         git \
         curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --shell /usr/sbin/nologin --uid 10001 nudo

COPY --from=build /src/target/release/nudo-server      /usr/local/bin/
COPY --from=build /src/target/release/nudo-web         /usr/local/bin/
COPY --from=build /src/target/release/nudo             /usr/local/bin/
COPY --from=build /src/target/release/nudo-mcp         /usr/local/bin/
COPY --from=build /src/target/release/nudo-all-in-one  /usr/local/bin/

# State lives here; mount a volume over it or the database is lost with the
# container.
RUN mkdir -p /var/lib/nudo && chown -R nudo:nudo /var/lib/nudo
VOLUME ["/var/lib/nudo"]

USER nudo
WORKDIR /var/lib/nudo

ENV NUDO_DB=/var/lib/nudo/nudo.db \
    NUDO_DATA_DIR=/var/lib/nudo/data \
    NUDO_WEB_ADDR=0.0.0.0:3000 \
    NUDO_GRPC_ADDR=127.0.0.1:50051 \
    RUST_LOG=info

EXPOSE 3000

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS http://127.0.0.1:3000/login >/dev/null || exit 1

# The all-in-one by default, since a single container is why someone reaches for
# the image. Override the command to run the halves separately.
CMD ["nudo-all-in-one"]

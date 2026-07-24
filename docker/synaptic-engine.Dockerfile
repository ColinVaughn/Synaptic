# syntax=docker/dockerfile:1.7

FROM rust:1.96.0-bookworm@sha256:5e2214abe154fe26e39f64488952e5c991eeed1d6d6da7cc8381ae83927f0cfc AS build
WORKDIR /source

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY bin ./bin
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/source/target,sharing=locked \
    cargo build --locked --release --package synaptic --bin synaptic \
    && cp /source/target/release/synaptic /tmp/synaptic \
    && strip /tmp/synaptic \
    && chmod 0555 /tmp/synaptic

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:66aa873a4a14fb164aa01296058efd8253744606d72715e45acface073359faa AS runtime
COPY LICENSE NOTICE /usr/share/licenses/synaptic/
COPY --from=build --chown=root:root /tmp/synaptic /usr/local/bin/synaptic
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/synaptic"]
CMD ["--help"]

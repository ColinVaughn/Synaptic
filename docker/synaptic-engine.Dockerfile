# syntax=docker/dockerfile:1.7

FROM rust:1.97.1-bookworm@sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97 AS build
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

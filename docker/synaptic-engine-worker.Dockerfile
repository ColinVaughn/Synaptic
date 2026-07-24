# syntax=docker/dockerfile:1.7

# The hosted graph worker runs on musl-based Alpine so its Git toolchain does
# not pull Perl into the production image. Build a separate static public
# engine artifact for that ABI; it remains FSL-1.1-ALv2 source-available code.
FROM rust:1.96.0-alpine3.23@sha256:5dc2af9dd547c33f64d5fc1d299ab93b51f39eaa16c426c476b990ce6caf5b3e AS build
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

FROM alpine:3.23@sha256:fd791d74b68913cbb027c6546007b3f0d3bc45125f797758156952bc2d6daf40 AS runtime
COPY LICENSE NOTICE /usr/share/licenses/synaptic/
COPY --from=build --chown=root:root /tmp/synaptic /usr/local/bin/synaptic
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/synaptic"]
CMD ["--help"]

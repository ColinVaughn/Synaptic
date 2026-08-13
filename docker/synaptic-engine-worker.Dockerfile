# syntax=docker/dockerfile:1.7

# The hosted graph worker runs on musl-based Alpine so its Git toolchain does
# not pull Perl into the production image. Build a separate static public
# engine artifact for that ABI; it remains AGPL-3.0-or-later code.
FROM rust:1.97.1-alpine3.23@sha256:c4a364ddbf684fe038e6fa6a4f25b30c8dc85247423e0e660676ece0d17be4a2 AS build
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

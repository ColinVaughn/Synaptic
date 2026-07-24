# Public engine container artifacts

Both Dockerfiles build only the FSL-1.1-ALv2-licensed, source-available
Synaptic Engine from the locked Rust workspace. They never include the
proprietary Platform source.

- `synaptic-engine.Dockerfile` produces the glibc engine on a non-root
  distroless runtime for the hosted runtime image.
- `synaptic-engine-worker.Dockerfile` produces a musl-linked engine on a
  non-root Alpine runtime for the hosted worker. That ABI lets the private
  worker use Alpine's Perl-free Git package.

Build them from the public repository root:

```text
docker build -f docker/synaptic-engine.Dockerfile -t synaptic-engine:0.7.0 .
docker build -f docker/synaptic-engine-worker.Dockerfile -t synaptic-engine-worker:0.7.0 .
```

Release automation must record the image manifest digest and SHA-256 of
`/usr/local/bin/synaptic` separately for each artifact, generate an SBOM and
provenance attestation, scan the final stage, and sign the published manifest.
Private images consume the public artifact only through a digest-pinned
BuildKit named context and verify the expected binary SHA-256 during their
build.

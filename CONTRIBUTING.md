# Contributing to Synaptic

Thank you for contributing to Synaptic.

## License for contributions

Unless you conspicuously state otherwise before submission, any contribution
you intentionally submit for inclusion in Synaptic is provided under the
Functional Source License, Version 1.1, ALv2 Future License
(`FSL-1.1-ALv2`), without additional terms or conditions. This is an
inbound-equals-outbound model: contributions receive the same non-competing-use
grant and automatic Apache 2.0 future license as the rest of Synaptic.

By contributing, you represent that you have the legal right to submit the
work. If your employer or another organization may own the work, obtain its
authorization before submitting it. Do not submit copied code or assets unless
their terms are compatible and you identify their source and license.

## Developer Certificate of Origin

Every commit must include a `Signed-off-by` trailer certifying the
[Developer Certificate of Origin 1.1](https://developercertificate.org/).
Add it with:

```console
git commit --signoff
```

The sign-off is a provenance certification, not a copyright assignment. A
separate contributor agreement may be introduced in the future if the project
needs broader relicensing rights.

## Before opening a pull request

Run the checks relevant to your change. For Rust changes, at minimum:

```console
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check licenses bans sources
```

Describe behavior and security-boundary changes in the pull request. Do not
commit secrets, proprietary B2B source, customer data, or generated credentials.

# Stability policy

Every crate in the workspace declares how much it promises about its public API. The tiers exist from the first commit so that the policy is in place before there is an API to break.

## Tiers

| Tier | Promise | Breaking changes | Semver gate |
|---|---|---|---|
| `stable` | The public API is supported | Only in a major release. Deprecated items stay for at least two minor releases and six months before removal | Any breaking change fails the pull request |
| `preview` | The API is expected to become stable, and feedback is wanted | Allowed in any release, but each one is declared: the pull-request title carries `!` and the changelog lists it | An undeclared breaking change fails the pull request |
| `experimental` | None. The crate may change or disappear | Allowed at any time | Not checked |
| `internal` | Not part of the product: repository tooling and benchmarks | Allowed at any time | Not checked; the crate is never published |

Before version 1.0, "major release" means what Cargo means for `0.x` versions: a change of the minor number.

## Declaring a tier

A crate states its tier twice, and `cargo xtask tier-check` requires the two to agree:

```toml
# Cargo.toml
[package.metadata.waveport]
stability = "experimental"
```

```rust
//! Crate documentation.
//!
//! Stability: experimental
```

The second line is there for readers: documentation shows the tier the manifest declares.

## Experimental items inside a more stable crate

A `stable` or `preview` crate can carry items that are not ready. They sit behind a Cargo feature whose name starts with `unstable-`, for example `unstable-moq`. Such a feature:

- is never enabled by default, directly or through another feature;
- is the only way a crate may depend on a less stable workspace crate: the dependency is optional and only `unstable-*` features enable it;
- is exempt from the crate's promise. Code that enables it accepts breaking changes in any release.

## Rules enforced by `cargo xtask tier-check`

| Rule | Fails when |
|---|---|
| TIER001 | A crate does not declare a tier, or declares an unknown one |
| TIER002 | A crate depends on a less stable workspace crate, other than through an optional dependency that only `unstable-*` features enable. Dev-dependencies are exempt |
| TIER003 | A crate's default features enable an `unstable-*` feature |
| TIER004 | An `internal` crate can be published |
| TIER005 | The crate documentation does not state the tier the manifest declares |

## The semver gate

`cargo xtask semver --baseline-rev <rev>` compares each `stable` and `preview` crate with the same crate at the base revision, using `cargo-semver-checks`:

- a `stable` crate must be compatible;
- a `preview` crate must be compatible unless the pull-request title declares a breaking change with `!`;
- `experimental` and `internal` crates, and crates that do not exist at the base revision, are skipped.

While every crate is experimental the gate has nothing to compare. So that it does not sit unproven, `cargo xtask semver --self-test` runs on every pull request: it builds three small crates and requires the tool to accept a compatible change and reject a breaking one.

## Changing a tier

Moving a crate to a more stable tier is a reviewed change to its manifest and documentation. Moving a `stable` crate to a less stable tier withdraws a promise, so it is a breaking change and waits for a major release.

## Current tiers

All product and testing crates are `experimental`: `waveport-domain`, `waveport-media`, `waveport-ports`, `waveport-runtime`, `waveport-control`, `waveport-protocol`, `waveport-sim`, `waveport-alloc-counter` and `waveport-testkit`. `xtask` and `waveport-benches` are `internal`.

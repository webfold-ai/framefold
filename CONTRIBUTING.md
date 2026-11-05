# Contributing to framefold

Thanks for helping. framefold is early and its scope is deliberately bounded, so please read the README's "What does not" section before proposing a feature.

## Before you start

- Open an issue to discuss anything larger than a bug fix.
- Keep changes focused. One concern per pull request.

## Checks

Every pull request must pass:

```sh
cargo fmt --all -- --check
cargo build --locked --all-targets
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Run `cargo deny check` and `cargo audit` if you change dependencies. Do not edit `Cargo.lock` by hand.

## Rendering changes

Rendering is a pure function of the frame index. A change that makes a frame depend on anything else needs a test that shows the determinism holds.

## Commits and pull requests

- Write clear commit messages in the imperative mood.
- Describe what changed and why in the pull request. Link any related issue.
- Maintainers review every change. Code owners are listed in `.github/CODEOWNERS`.

## Licence

By contributing, you agree that your contribution is licensed under the Apache License, Version 2.0.

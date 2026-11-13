# Releasing framefold

1. Update `version` in `Cargo.toml` and add a `## [x.y.z]` section to `CHANGELOG.md`, with `## [Unreleased]` above it.
2. Merge to `main`. CI must pass.
3. Create and push the tag `vX.Y.Z` matching `Cargo.toml`. Only a maintainer creates tags.
4. The release workflow builds, tests, signs and attests the artifacts, and creates a **draft** release.
5. Review the draft's assets and notes, then publish it manually.

Do not create tags from automation. A tag starts the release workflow.

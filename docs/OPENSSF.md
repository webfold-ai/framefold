# OpenSSF and supply-chain checks

This file records what the repository does for the OpenSSF Scorecard checks and where a person must act. It does not claim a score.

| Check | Where | Status |
|---|---|---|
| Pinned-Dependencies | every `uses:` in `.github/workflows/` | Pinned to full SHAs, verified with `git ls-remote`. |
| SAST | `.github/workflows/codeql.yml` | CodeQL on Rust. Needs GitHub Advanced Security on a private repo. |
| Signed-Releases | `.github/workflows/release.yml` | cosign bundles and SLSA provenance on tag builds. Draft releases only. |
| Dependency-Update-Tool | `.github/dependabot.yml` | Cargo and GitHub Actions, weekly. |
| Token-Permissions | every workflow | Top-level `contents: read`; write scopes only where needed. |
| Security-Policy | `SECURITY.md` | Present. |
| License | `LICENSE` | Apache-2.0. |
| Fuzzing | none | Not present. |
| Branch-Protection | GitHub settings | Not configured by this repository. Owner action. |
| Code-Review | GitHub settings | Depends on branch protection. Owner action. |
| CII-Best-Practices | bestpractices.dev | Not registered. Owner action. |

## Owner actions

1. Enable dependency graph and GitHub Advanced Security, or make the repository public, so CodeQL and dependency review can run.
2. Set the default branch to `main` in GitHub settings.
3. Configure branch protection requiring the `Rust checks` status check and code-owner review.
4. Register the project with bestpractices.dev.

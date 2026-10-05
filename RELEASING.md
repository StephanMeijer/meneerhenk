# Releasing

Henk is not published to crates.io. Every crate is `publish = false`, shares
the workspace version, and a release is one `vX.Y.Z` tag with one GitHub
Release, signed Linux binaries and a signed container image.

## How a release happens

1. Every push to `main` runs `release-plz-pr` in `ci.yml`. It opens or
   updates a release PR (`chore: release vX.Y.Z`) that bumps
   `[workspace.package].version` and adds the commits since the last tag to
   `CHANGELOG.md`, grouped by conventional-commit type. The next version
   follows from those types (`feat` a minor bump, `fix` a patch; below 1.0 a
   breaking change is a minor bump).
2. Merging the release PR runs `release-plz-release`. release-plz runs
   git-only (`release-plz.toml`): it compares the workspace version with the
   last `v*` tag and, when it is ahead, pushes the tag and creates the GitHub
   Release with the changelog entry as its body.
3. That job then dispatches `release.yml` for the tag, which builds the
   artifacts below and appends them to the release.

Both release jobs run only after `fmt`, `clippy`, `test` and `deny` pass on
the same push.

`release_always = true` means any push to `main` whose version is ahead of
the last tag releases, not only the merge of a release PR. That makes a
missed release heal on the next push, and it means a version bump outside a
release PR releases on merge. Keep version bumps in release PRs.

## What is published

- `ghcr.io/stephanmeijer/meneerhenk:X.Y.Z`, `:X.Y` and `:latest` (not for a
  prerelease tag such as `v1.0.0-rc.1`): linux/amd64 and linux/arm64 in one
  manifest list, signed with cosign (keyless) and with SLSA build provenance.
- On the GitHub Release: `henk-x86_64-unknown-linux-gnu.tar.xz` and
  `henk-aarch64-unknown-linux-gnu.tar.xz`, `henk-installer.sh`, `sha256.sum`
  and per-file checksums, each with a cosign `.bundle` and SLSA provenance.

## Verifying

```sh
cosign verify ghcr.io/stephanmeijer/meneerhenk:X.Y.Z \
  --certificate-identity-regexp '^https://github.com/StephanMeijer/meneerhenk/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
gh attestation verify oci://ghcr.io/stephanmeijer/meneerhenk:X.Y.Z --owner StephanMeijer

cosign verify-blob henk-x86_64-unknown-linux-gnu.tar.xz \
  --bundle henk-x86_64-unknown-linux-gnu.tar.xz.bundle \
  --certificate-identity-regexp '^https://github.com/StephanMeijer/meneerhenk/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
gh attestation verify henk-x86_64-unknown-linux-gnu.tar.xz --owner StephanMeijer
```

## Repository settings this needs

- Settings, Actions, General: "Allow GitHub Actions to create and approve
  pull requests", or `release-plz-pr` cannot open the release PR.
- After the first release, make the `meneerhenk` package on GHCR public.

A release PR opened with `GITHUB_TOKEN` does not trigger CI on itself;
the release job runs after the merge, on the push to `main`.

## Running a release by hand

`gh workflow run release.yml --ref vX.Y.Z` rebuilds and re-attaches the
artifacts for an existing tag. The workflow refuses a ref that is not a
`v*` tag.

## Versions that move by hand

- `release-plz` (`version:` on both steps in `ci.yml`), cargo-dist
  (`cargo-dist-version` in `Cargo.toml` and the installer URL in
  `release.yml`; `dist plan` shows the matrix the build jobs expect).
- `cargo-deny` and `cargo-machete` in the `tool:` inputs of `ci.yml` and
  `advisories.yml`; an update bot bumps the action SHA but cannot see these.
- cosign (`cosign-release:` in `release.yml`).

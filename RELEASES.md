# Releasing bpm

Operational runbook for cutting a release. Every step up to the publish
buttons runs from this repo; the publishes themselves need the owner's
accounts (GitHub is already wired via CI).

## 1. Cut the release commit

1. Confirm CI is green on `main` (the real-world corpus gate is part of
   the release bar — it runs the ten pinned repos with exact placement
   counts).
2. Bump `version` in `Cargo.toml`.
3. If the release changes installation semantics, revisit the
   "Lockfile stability" section of the README: within `bpm.lock` v2 only
   additive informational fields are allowed; anything semantic needs a
   new `lockfileVersion` and a migration.
4. Locally: `cargo build --release && ./target/release/bpm --version`
   (must report the new version), then the full gate:
   `cargo test --workspace && cargo fmt --all --check && cargo clippy
   --workspace --all-targets --all-features -- -D warnings`.
5. Write the release notes to a file (validation numbers, what changed,
   upgrade notes — see the v0.5.0 notes for the shape) and commit with
   the message `vX.Y.Z: <theme>`.

## 2. Tag and publish binaries

```bash
git push origin main
git tag -a vX.Y.Z -F /path/to/notes.md
git push origin vX.Y.Z
```

The `release` workflow builds four signed tarballs (macOS arm64/x64,
Linux x86_64/arm64), signs `SHA256SUMS` with the release environment's
`RELEASE_SIGNING_PRIVATE_KEY`, and creates the GitHub release with empty
notes. Attach the real notes immediately:

```bash
gh release edit vX.Y.Z --repo Lbniese/bpm --notes-file /path/to/notes.md
```

Verify: the release lists 4 tarballs + `SHA256SUMS` + `SHA256SUMS.sig`,
and `gh release download ... --pattern SHA256SUMS*` then
`openssl dgst -sha256 -verify .github/release-signing-public.pem
-signature SHA256SUMS.sig SHA256SUMS` passes.

`install.sh` follows `latest` automatically — smoke it without sudo:

```bash
BPM_VERSION=vX.Y.Z BPM_INSTALL_DIR="$(mktemp -d)" \
  curl -fsSL https://raw.githubusercontent.com/Lbniese/bpm/main/install.sh | sh
```

## 3. Publish to crates.io

The package is **`bloom-pm`** (the `bpm` crate name has belonged to an
unrelated crate since 2022). The installed binary is still `bpm` — the
lib and bin names are pinned in `Cargo.toml`.

```bash
cargo publish --dry-run   # validates metadata + packaging
cargo publish             # needs a crates.io token with publish rights
```

`cargo install bloom-pm` is also the Windows story until prebuilt
Windows binaries exist.

## 4. Update the Homebrew tap

The formula source lives at `packaging/homebrew/Formula/bpm.rb` in this
repo. To publish, maintain a tap repo (e.g. `Lbniese/homebrew-tap`) and,
per release:

1. Copy the formula into the tap's `Formula/bpm.rb`.
2. Bump `version` (the tarball URLs embed it) and replace all four
   `sha256` lines with the values from the release's signed
   `SHA256SUMS`.
3. In the tap: `brew audit --formula bpm` and
   `brew install bpm && bpm --version`, then commit.

Users then install with:
`brew tap Lbniese/tap https://github.com/Lbniese/homebrew-tap && brew install bpm`
(or `brew install Lbniese/tap/bpm`).

## Not yet distributed

- **winget**: blocked on shipping prebuilt Windows binaries. The release
  workflow's asset-set check also hard-codes the four Unix tarballs;
  adding a Windows target means updating that check, and winget review
  will want stable, ideally signed, artifacts.
- **npm wrapper** (`npm i -g bpm` shim that fetches the platform
  tarball): possible later; deferred until there's demand.

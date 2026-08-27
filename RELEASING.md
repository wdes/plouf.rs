# Releasing plouf.rs

Releases are cut from a signed `v*` git tag. Pushing the tag triggers
`.github/workflows/release.yml`, which cross-compiles the artifacts and opens a
**draft** GitHub release for you to review and publish.

## Steps

1. **Be on an up-to-date, green `main`.**

   ```sh
   git switch main && git pull --ff-only
   ```

   Check the latest `build` run is green before tagging.

2. **Bump the version in the two Cargo files** (drop the `-dev` suffix):

   - `Cargo.toml` — `version = "X.Y.Z"`.
   - `Cargo.lock` — the `[[package]]` entry `name = "plouf-rs"`, `version = "X.Y.Z"`.
     `cargo build` regenerates it, or edit the one line by hand.

3. **Commit the bump.**

   ```sh
   git commit -am "chore: release vX.Y.Z"
   git push origin main
   ```

4. **Tag it, signed and annotated, then push the tag** (this is what fires the
   release):

   ```sh
   git tag -a -s -m "vX.Y.Z" vX.Y.Z
   git push origin vX.Y.Z
   ```

   Signing uses `git config user.signingkey`; the tag name must match `v*`.

5. **Let the `release` workflow run.** On the tag it:

   - runs a reproducibility check on the amd64 `.deb`;
   - cross-compiles a `.deb` per Debian arch (amd64 + arm64) with
     `cargo-zigbuild`, and extracts the raw `plouf-rs-<arch>` binary from each;
   - builds the static musl and the macOS (aarch64 + x86_64) binaries;
   - runs the end-to-end check against the mock fixtures;
   - attaches SLSA build-provenance attestations to every artifact;
   - publishes all of it to a **draft** GitHub release.

   (`workflow_dispatch` runs the same matrix without publishing — handy to
   exercise the cross-compile without cutting a release.)

6. **Review and publish the draft release** on GitHub. Confirm the `.deb`s, the
   raw per-arch binaries, and the attestations are attached, then hit publish.

7. **Re-open development.** Bump the two Cargo files to the next dev version and
   commit:

   ```sh
   # e.g. X.Y.(Z+1)-dev
   git commit -am "chore: X.Y.(Z+1)-dev"
   git push origin main
   ```

## Notes

- The release is a **draft** — nothing is public until you publish it, so a bad
  tag is recoverable (delete the tag + the draft, fix, re-tag).
- Consumers install the `.deb` (`apt install ./plouf-rs_X.Y.Z-1_amd64.deb`) or
  grab a raw binary; both carry the SLSA attestation
  (`gh attestation verify <file> --repo wdes/plouf.rs`).
- `RELEASING.md` and the CI/agent tooling are excluded from the published crate
  (see `exclude` in `Cargo.toml`).

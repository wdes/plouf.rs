# Releasing plouf.rs

Releases are cut from a signed `v*` git tag. Pushing the tag triggers
`.github/workflows/release.yml`, which cross-compiles the artifacts and opens a
**draft** GitHub release for you to review and publish.

## Steps

1. **Be on an up-to-date, green `main`.**

   ```sh
   git switch main && git pull --ff-only
   ```

   Check the latest `build` run is green before releasing.

2. **Run the release driver** with the version you are cutting (a leading `v` is
   fine). The release version is your choice — pass a patch, minor, or major:

   ```sh
   scripts/release.sh 0.4.0
   ```

   In one shot it bumps every pinned version (`Cargo.toml`, `Cargo.lock`,
   and the `.deb` URL in `README.md` + the baked `SKILL.md`) to `X.Y.Z`, commits
   `chore: release vX.Y.Z` and pushes `main`, then creates a **signed annotated**
   tag `vX.Y.Z` and pushes it (which fires the release build). It requires a
   clean tree on `main` and a signing key (`git config user.signingkey`).

   Finally it **always re-opens development on the next patch**: it bumps to
   `X.Y.(Z+1)-dev`, commits `chore: X.Y.(Z+1)-dev`, and pushes `main`. So after
   releasing `0.4.0` the tree is left on `0.4.1-dev`; the next release version is
   again your explicit choice at the next `scripts/release.sh` call.

3. **Let the `release` workflow run.** On the tag it:

   - runs a reproducibility check on the amd64 `.deb`;
   - cross-compiles a `.deb` per Debian arch (amd64 + arm64) with
     `cargo-zigbuild`, and extracts the raw `plouf-rs-<arch>` binary from each;
   - builds the static musl and the macOS (aarch64 + x86_64) binaries;
   - runs the end-to-end check against the mock fixtures;
   - attaches SLSA build-provenance attestations to every artifact;
   - publishes all of it to a **draft** GitHub release.

   (`workflow_dispatch` runs the same matrix without publishing — handy to
   exercise the cross-compile without cutting a release.)

4. **Review and publish the draft release** on GitHub. Confirm the `.deb`s, the
   raw per-arch binaries, and the attestations are attached, then hit publish.
   (Development is already re-opened on the next `-dev` by step 2 -- nothing more
   to do here.)

## Notes

- The release is a **draft** — nothing is public until you publish it, so a bad
  tag is recoverable (delete the tag + the draft, fix, re-tag).
- Consumers install the `.deb` (`apt install ./plouf-rs_X.Y.Z-1_amd64.deb`) or
  grab a raw binary; both carry the SLSA attestation
  (`gh attestation verify <file> --repo wdes/plouf.rs`).
- `RELEASING.md` and the CI/agent tooling are excluded from the published crate
  (see `exclude` in `Cargo.toml`).

# Releasing marina

## Before tagging

1. Bump `version` in `Cargo.toml` and run `cargo check` so `Cargo.lock` follows.
2. Merge that to `main` through a pull request (main requires CI to pass).

## Tag

```sh
git tag vX.Y.Z
git push origin vX.Y.Z
```

The tag runs `.github/workflows/release.yml`:

- `release` builds each target and attaches the archives to a GitHub Release.
- `publish-crate` publishes `marina-tui` to crates.io through Trusted
  Publishing (OIDC; no stored token).
- `homebrew-formula` opens the Homebrew tap pull request (below).
- `attest` signs build provenance for the release assets and publishes
  `SHA256SUMS`.

## Publish the Homebrew bottles

The release's `homebrew-formula` job opens a pull request on
[danfry1/homebrew-tap](https://github.com/danfry1/homebrew-tap) (branch
`marina-<version>`) that points `marina.rb` at the tag's source archive. The tap's CI
(`brew test-bot`) builds and tests bottles on it for Apple Silicon macOS 15+,
x86_64 Linux and ARM Linux, in a few minutes.

1. Wait for that pull request's checks to pass.
2. Run the publish command the bot posts as a comment on it:

   ```sh
   gh workflow run publish.yml -R danfry1/homebrew-tap -f pull_request=<N> -f head_sha=<sha>
   ```

   `brew pr-pull` uploads the bottles to a release in the tap, attests them,
   commits the bottle block and closes the pull request.

**Never merge the bot's pull request with the merge button.** That ships a
formula without bottles, so every install compiles from source until the next
publish.

## Check the release

- The workflow run is green, including `attest` (signed build provenance for
  every release asset): `gh attestation verify <file> --repo danfry1/marina`.
- `brew update && brew upgrade marina` pours a bottle (`brew info marina` shows the
  new version).

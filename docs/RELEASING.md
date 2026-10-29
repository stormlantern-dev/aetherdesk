# Releasing

For now: alphas, as GitHub pre-releases and Homebrew casks in
[stormlantern-dev/homebrew-tap](https://github.com/stormlantern-dev/homebrew-tap).

1. **Version.** Set `version` in `[workspace.package]` of `Cargo.toml`
   (e.g. `0.1.0-alpha.1`), commit through a PR, merge.
2. **Tag** the merge on `main` and push the tag:

   ```sh
   git tag v0.1.0-alpha.1 && git push origin v0.1.0-alpha.1
   ```

   The `Release` workflow checks that tag and version match, creates the
   GitHub release as a pre-release, and attaches:
   - `aetherdesk-<version>-x86_64-linux.tar.gz` (client, built in the Arch
     devcontainer image) and its `.sha256`;
   - `aetherdesk-host-<version>-arm64-macos.tar.gz` (host, signed ad hoc) and
     its `.sha256`.
3. **Casks.** With a checkout of the tap:

   ```sh
   packaging/homebrew/casks.sh 0.1.0-alpha.1 ../homebrew-tap
   ```

   fills `Casks/aetherdesk.rb` and `Casks/aetherdesk-host.rb` from the
   templates in `packaging/homebrew/` and the release's checksums. It
   doesn't commit: check the diff, then commit to a branch and open a PR on
   the tap. Its `main` is protected by a ruleset, so a direct push is
   rejected. Merging that PR publishes the casks.
4. **Try it** on both machines:

   ```sh
   brew install --cask stormlantern-dev/tap/aetherdesk        # Linux
   brew install --cask stormlantern-dev/tap/aetherdesk-host   # Mac
   aether-agent install                                       # Mac, then the permissions
   ```

Pushing the tag and merging the tap PR publish: both are the maintainer's call.

## What "alpha" means here

- **Mac:** signed ad hoc. macOS asks for Screen Recording and Accessibility
  again after every update, unless the user has a code-signing identity
  "AetherDesk Dev" (or `AETHERDESK_IDENTITY`), which `aether-agent install`
  then signs with. Developer ID and notarisation: #28.
- **Linux:** the client is linked against current Arch's SDL3 and FFmpeg, so
  it runs on an up-to-date Arch or CachyOS. AUR packages and a Flatpak: #37.

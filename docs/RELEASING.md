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
   - `aetherdesk-host-<version>-arm64-macos.tar.gz` (host, signed with the Developer ID and notarized) and
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

- **Mac:** a bare `aether-agent` binary, signed with the Developer ID and
  notarized (#74), so Screen Recording and Accessibility, once granted,
  survive updates. The `.app` bundle, a `.pkg` and the "Remote Desktop"
  entitlement: #28.
- **Linux:** the client is linked against current Arch's SDL3 and FFmpeg, so
  it runs on an up-to-date Arch or CachyOS. AUR packages and a Flatpak: #37.

## Signing the Mac host

The `Release` workflow's Mac job signs `aether-agent` with a **Developer ID
Application** certificate and notarizes it, through
`packaging/macos/sign-notarize.sh`. It fails without the secrets below: an
ad hoc build would make every tester grant the permissions again after each
update.

One-time setup, by the Apple Developer account holder:

1. **Certificate.** On a Mac: Keychain Access → Certificate Assistant →
   Request a Certificate From a Certificate Authority, saved to disk. Then at
   developer.apple.com → Certificates → + → *Developer ID Application*,
   upload that request, download the certificate and open it. In Keychain
   Access, export the certificate together with its private key as a `.p12`,
   with a password.
2. **Notary key.** App Store Connect → Users and Access → Integrations →
   App Store Connect API → Team Keys → +, access *Developer*. Download the
   `.p8` (only possible once) and note its Key ID and the Issuer ID.
3. **Environment.** GitHub → Settings → Environments → New environment
   `release`; under *Deployment branches and tags* allow only tags matching
   `v*`.
4. **Secrets** in that environment (`gh` asks for the values that aren't
   piped in, so nothing lands in shell history):

   ```sh
   base64 -i developer-id.p12 | gh secret set MACOS_DEVELOPER_ID_P12 --env release
   gh secret set MACOS_DEVELOPER_ID_P12_PASSWORD --env release
   base64 -i AuthKey_XXXXXXXXXX.p8 | gh secret set NOTARY_API_KEY_P8 --env release
   gh secret set NOTARY_API_KEY_ID --env release
   gh secret set NOTARY_API_ISSUER_ID --env release
   ```

Then delete the exported `.p12` and keep the `.p8` somewhere safe (or
delete it too: a new key can be made at any time).

**By hand on a Mac**, with the certificate in the login keychain:

```sh
xcrun notarytool store-credentials aetherdesk \
  --key AuthKey_XXXXXXXXXX.p8 --key-id <key id> --issuer <issuer id>
cargo build --release -p aetherdesk-mac-host
NOTARY_PROFILE=aetherdesk packaging/macos/sign-notarize.sh target/release/aether-agent
target/release/aether-agent install
```

`SKIP_NOTARIZE=1` only signs, to try the hardened runtime quickly.
`aether-agent install` keeps a valid Developer ID signature; it re-signs
other builds as before.

# Releasing

For now: alphas, as GitHub pre-releases, Homebrew casks in
[stormlantern-dev/homebrew-tap](https://github.com/stormlantern-dev/homebrew-tap),
and for the Mac an installer package that updates itself (Sparkle).

1. **Version.** Set `version` in `[workspace.package]` of `Cargo.toml`
   (e.g. `0.1.0-alpha.1`), commit through a PR, merge. Mac versions must be
   `M.m.p` or `M.m.p-(alpha|beta|rc).N` with N below 100: the app's
   `CFBundleVersion`, which macOS and Sparkle compare, is made from it
   (`crates/mac-host/src/bundle.rs`; 0.1.0-alpha.4 is 0.1.104).
2. **Tag** the merge on `main` and push the tag:

   ```sh
   git tag v0.1.0-alpha.1 && git push origin v0.1.0-alpha.1
   ```

   The `Release` workflow checks that tag and version match, creates the
   GitHub release as a pre-release, and attaches:
   - `aetherdesk-<version>-x86_64-linux.tar.gz` (client, built in the Arch
     devcontainer image) and its `.sha256`;
   - `aetherdesk-<version>-x86_64.flatpak` (client as a Flatpak bundle, built
     by `packaging/flatpak/build.sh` on runtime `org.freedesktop.Platform`)
     and its `.sha256`;
   - `AetherDesk-<version>.pkg` (host installer: AetherDesk.app signed with
     the Developer ID and notarized, in a package signed with the Developer
     ID Installer certificate, notarized and stapled) and its `.sha256`;
   - `aetherdesk-host-<version>-arm64-macos.tar.gz` (the same AetherDesk.app,
     for Homebrew) and its `.sha256`;
   - `appcast.xml`: Sparkle's feed for this release, the package's EdDSA
     signature and the release notes, signed itself. Not published yet.

   All Mac parts come from `packaging/macos/release.sh` (which uses
   `app-bundle.sh`, `sign-notarize.sh` and `build-pkg.sh`) and
   `packaging/macos/appcast.sh`.
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

   or, on the Mac, open the release's `.pkg`.
5. **Appcast.** Once the release works, tell the installed apps about it:

   ```sh
   gh workflow run appcast.yml -f tag=v0.1.0-alpha.1
   ```

   The `Appcast` workflow takes `appcast.xml` from that release and
   publishes it on GitHub Pages at
   <https://stormlantern-dev.github.io/aetherdesk/appcast.xml>, the
   `SUFeedURL` in every app. Within a day each one offers the update. To go
   back, run it again with the previous tag (an app never offers an older
   version, but stops offering the bad one).

Pushing the tag, merging the tap PR and running the Appcast workflow
publish: all three are the maintainer's call.

### Why GitHub Pages for the appcast

The feed needs one URL that never changes. A release asset can't give one:
`releases/latest` skips pre-releases, which all alphas are, and a rolling
"appcast" release would show up among the real ones. Pages serves a fixed
URL over HTTPS from a deployment, so no branch has to be pushed to (`main`
is protected) and publishing stays a separate, deliberate step. The feed
and the package are both EdDSA-signed in the release environment, so Pages
only has to deliver them: the app refuses an altered feed
(`SURequireSignedFeed`) or package.

## What "alpha" means here

- **Mac:** AetherDesk.app, signed with the Developer ID and notarized, so
  Screen Recording and Accessibility, once granted to the app, survive
  updates. From macOS 26.1 on bare executables don't show up in the Screen
  Recording list, hence the app (#28). The "Remote Desktop" entitlement:
  #28.
- **Linux:** the client is linked against current Arch's SDL3 and FFmpeg, so
  it runs on an up-to-date Arch or CachyOS. Anywhere else: the Flatpak bundle
  from the release. AUR packages: #37.

## Signing the Mac host

The `Release` workflow's Mac job signs AetherDesk.app with a **Developer ID
Application** certificate (hardened runtime, nested code first:
`packaging/macos/sign-app.sh`), notarizes and staples it, then signs the
package with a **Developer ID Installer** certificate and notarizes and
staples that too. It signs the package and the appcast with the **Sparkle
EdDSA key**. It fails without the secrets and the variable below: an ad hoc
build would make every tester grant the permissions again after each
update, and an app without the public key can't check updates.

One-time setup, by the Apple Developer account holder:

1. **Certificates.** On a Mac: Keychain Access → Certificate Assistant →
   Request a Certificate From a Certificate Authority, saved to disk. Then at
   developer.apple.com → Certificates → + → *Developer ID Application*,
   upload that request, download the certificate and open it. Do the same
   for *Developer ID Installer* (a second request is fine). Also open
   Apple's intermediate certificate
   <https://www.apple.com/certificateauthority/DeveloperIDG2CA.cer>; without
   it `codesign` fails with "unable to build chain" and
   `errSecInternalComponent`. In Keychain Access, export each certificate
   together with its private key as a `.p12`, with a password.
2. **Notary key.** App Store Connect → Users and Access → Integrations →
   App Store Connect API → Team Keys → +, access *Developer*. Download the
   `.p8` (only possible once) and note its Key ID and the Issuer ID.
3. **Sparkle key.** On the Mac, from Sparkle's release (the version
   `packaging/macos/fetch-sparkle.sh` pins):

   ```sh
   packaging/macos/fetch-sparkle.sh /tmp/sparkle      # prints the folder
   /tmp/sparkle/Sparkle-<version>/bin/generate_keys    # prints the public key
   /tmp/sparkle/Sparkle-<version>/bin/generate_keys -x sparkle-private.key
   ```

   The first run creates the key pair in the login keychain and prints the
   public key; `-x` exports the private key to a file. Keep the keychain
   entry (or another safe copy): **losing the private key means installed
   apps can never be updated again** except by installing a new `.pkg` by
   hand.
4. **Environment.** GitHub → Settings → Environments → New environment
   `release`; under *Deployment branches and tags* allow only tags matching
   `v*`.
5. **Secrets and variable** in that environment (`gh` asks for the values
   that aren't piped in, so nothing lands in shell history):

   ```sh
   base64 -i developer-id.p12 | gh secret set MACOS_DEVELOPER_ID_P12 --env release
   gh secret set MACOS_DEVELOPER_ID_P12_PASSWORD --env release
   base64 -i developer-id-installer.p12 | gh secret set MACOS_DEVELOPER_ID_INSTALLER_P12 --env release
   gh secret set MACOS_DEVELOPER_ID_INSTALLER_P12_PASSWORD --env release
   base64 -i AuthKey_XXXXXXXXXX.p8 | gh secret set NOTARY_API_KEY_P8 --env release
   gh secret set NOTARY_API_KEY_ID --env release
   gh secret set NOTARY_API_ISSUER_ID --env release
   gh secret set SPARKLE_ED_PRIVATE_KEY --env release < sparkle-private.key
   gh variable set SPARKLE_ED_PUBLIC_KEY --env release   # the public key generate_keys printed
   ```

   The public key goes into every app's Info.plist (`SUPublicEDKey`); the
   build checks it looks like one and fails without it. It must belong to
   the private key, or no app accepts the updates.
6. **Pages.** GitHub → Settings → Pages → Source: *GitHub Actions*. The
   `github-pages` environment it creates only lets the default branch
   deploy, which is where the Appcast workflow runs from.

Then delete the exported `.p12` files and `sparkle-private.key`, and keep
the `.p8` somewhere safe (or delete it too: a new key can be made at any
time).

**By hand on a Mac**, with the certificates in the login keychain:

```sh
xcrun notarytool store-credentials aetherdesk \
  --key AuthKey_XXXXXXXXXX.p8 --key-id <key id> --issuer <issuer id>
NOTARY_PROFILE=aetherdesk SIGNING=developer-id SPARKLE_ED_PUBLIC_KEY=<public key> \
  packaging/macos/release.sh 0.1.0-alpha.1 dist
```

Without `SIGNING=developer-id`, `release.sh` signs ad hoc and leaves the
package unsigned, to try the packaging. CI builds the app and package that
way (`app-bundle.sh`, `build-pkg.sh`) on every pull request that touches the
host, and installs and uninstalls the package on the runner. `SKIP_NOTARIZE=1`
only signs, to try the hardened runtime quickly. `aether-agent install`
keeps a valid Developer ID signature; it re-signs other builds as before.

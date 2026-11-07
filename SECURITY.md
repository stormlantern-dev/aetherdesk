# Security

AetherDesk gives remote control over a computer, so security reports are
welcome and taken seriously.

## Reporting a vulnerability

Please report it privately via
[GitHub's private vulnerability reporting](https://github.com/stormlantern-dev/aetherdesk/security/advisories/new),
not in a public issue. Include what you found, how to reproduce it, and which
version or commit you tested.

You'll get an answer within a week. Once a fix is released, the advisory is
published with credit to you, unless you'd rather stay anonymous.

## Supported versions

AetherDesk is in alpha. Only the latest release gets fixes.

## Verifying a release

Every release artifact comes with a build provenance attestation and an SBOM,
both signed by GitHub Actions. To check that a download was built by this
repository's release workflow:

```sh
gh attestation verify aetherdesk-<version>-x86_64-linux.tar.gz --repo stormlantern-dev/aetherdesk
```

The SBOMs (CycloneDX JSON, Rust dependencies) are attached to the release as
`*.cdx.json`.

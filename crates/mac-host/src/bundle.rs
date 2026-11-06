//! The host as an app bundle, `AetherDesk.app` (#28, #115). From macOS 26.1
//! on, bare executables don't show up in the Screen Recording list, so the
//! installed daemon and agents run `aether-agent` from inside the bundle and
//! the permissions attach to the app. The bundle is also the home of the
//! menu-bar item and its updater ([`crate::menu`]).
//!
//! What's here is plain data (paths, the Info.plist, version numbers), so it
//! is tested on Linux too; `packaging/macos/app-bundle.sh` builds the bundle
//! with `aether-agent _info-plist`.

use std::path::{Path, PathBuf};

/// The bundle identifier, which is also the code-signing identifier: the one
/// the bare binary was signed with before, so the designated requirement
/// stays the same.
pub const BUNDLE_ID: &str = "nl.stormlantern.aetherdesk.agent";
/// Where the installer and `aether-agent install` put the app: where people
/// look for it, and where System Settings' + button starts.
pub const APP: &str = "/Applications/AetherDesk.app";
/// The installed binary: what launchd runs.
pub const BIN: &str = "/Applications/AetherDesk.app/Contents/MacOS/aether-agent";
/// The appcast Sparkle reads (GitHub Pages; docs/RELEASING.md).
pub const FEED_URL: &str = "https://stormlantern-dev.github.io/aetherdesk/appcast.xml";
/// The audio device, inside the bundle (where nested code is signed and
/// checked as such); installed from there.
pub const DRIVER_IN_BUNDLE: &str = "Contents/PlugIns/AetherDesk.driver";

/// The `.app` this executable lies in, if it does
/// (`X.app/Contents/MacOS/<exe>`).
pub fn enclosing_bundle(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    let is_app = app.extension().is_some_and(|e| e == "app");
    (macos.file_name()? == "MacOS" && contents.file_name()? == "Contents" && is_app)
        .then(|| app.to_path_buf())
}

/// Who installed the host, from where `aether-agent install` ran:
/// Homebrew's casks live in its `Caskroom`. Homebrew installs don't use the
/// app's updater, so the two never fight.
pub fn install_source(exe: &Path) -> &'static str {
    if exe.components().any(|c| c.as_os_str() == "Caskroom") {
        "homebrew"
    } else {
        "manual"
    }
}

/// `CFBundleVersion` for a Cargo version: three integers that sort like the
/// versions do, which is what macOS and Sparkle compare. The patch number
/// carries the pre-release: 0.1.0-alpha.4 is 0.1.104, -beta.N 0.1.2NN,
/// -rc.N 0.1.3NN, 0.1.0 itself 0.1.900, and 0.1.1-alpha.1 0.1.1101.
pub fn build_number(version: &str) -> Result<String, String> {
    let bad = || format!("version {version:?}: expected M.m.p or M.m.p-(alpha|beta|rc).N");
    let (release, pre) = match version.split_once('-') {
        Some((r, p)) => (r, Some(p)),
        None => (version, None),
    };
    let parts: Vec<u64> = release
        .split('.')
        .map(|p| p.parse().map_err(|_| bad()))
        .collect::<Result<_, _>>()?;
    let [major, minor, patch] = parts[..] else {
        return Err(bad());
    };
    let rank = match pre {
        None => 900,
        Some(pre) => {
            let (stage, n) = pre.split_once('.').ok_or_else(bad)?;
            let n: u64 = n.parse().map_err(|_| bad())?;
            let base = match stage {
                "alpha" => 100,
                "beta" => 200,
                "rc" => 300,
                _ => return Err(bad()),
            };
            if n >= 100 {
                return Err(bad());
            }
            base + n
        }
    };
    Ok(format!("{major}.{minor}.{}", patch * 1000 + rank))
}

/// What a public EdDSA key from Sparkle's `generate_keys` looks like:
/// 32 bytes in base64.
pub fn is_ed_public_key(key: &str) -> bool {
    key.len() == 44
        && key.ends_with('=')
        && key[..43]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The app's `Info.plist`. Without `ed_key` it has no updater settings, and
/// the menu says updates are off (a development build).
pub fn info_plist(version: &str, feed: &str, ed_key: Option<&str>) -> Result<String, String> {
    let build = build_number(version)?;
    let sparkle = match ed_key {
        Some(key) => {
            if !is_ed_public_key(key) {
                return Err(format!(
                    "{key:?} is not a public EdDSA key from Sparkle's generate_keys"
                ));
            }
            format!(
                "\t<key>SUFeedURL</key>\n\t<string>{feed}</string>\n\
                 \t<key>SUPublicEDKey</key>\n\t<string>{key}</string>\n\
                 \t<key>SURequireSignedFeed</key>\n\t<true/>\n\
                 \t<key>SUVerifyUpdateBeforeExtraction</key>\n\t<true/>\n\
                 \t<key>SUEnableAutomaticChecks</key>\n\t<true/>\n\
                 \t<key>SUScheduledCheckInterval</key>\n\t<integer>86400</integer>\n",
                feed = xml(feed),
                key = xml(key),
            )
        }
        None => String::new(),
    };
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleDisplayName</key>
	<string>AetherDesk</string>
	<key>CFBundleExecutable</key>
	<string>aether-agent</string>
	<key>CFBundleIconFile</key>
	<string>AetherDesk</string>
	<key>CFBundleIdentifier</key>
	<string>{BUNDLE_ID}</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>AetherDesk</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>{version}</string>
	<key>CFBundleVersion</key>
	<string>{build}</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.utilities</string>
	<key>LSMinimumSystemVersion</key>
	<string>14.0</string>
	<key>LSUIElement</key>
	<true/>
	<key>NSPrincipalClass</key>
	<string>NSApplication</string>
{sparkle}</dict>
</plist>
"#,
        version = xml(version),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_numbers_sort_like_versions() {
        let versions = [
            "0.1.0-alpha.1",
            "0.1.0-alpha.4",
            "0.1.0-alpha.12",
            "0.1.0-beta.1",
            "0.1.0-rc.2",
            "0.1.0",
            "0.1.1-alpha.1",
            "0.1.1",
            "0.2.0-alpha.1",
            "1.0.0",
        ];
        let numbers: Vec<Vec<u64>> = versions
            .iter()
            .map(|v| {
                build_number(v)
                    .unwrap()
                    .split('.')
                    .map(|p| p.parse().unwrap())
                    .collect()
            })
            .collect();
        assert!(numbers.windows(2).all(|w| w[0] < w[1]), "{numbers:?}");
        assert_eq!(build_number("0.1.0-alpha.4").unwrap(), "0.1.104");
        assert_eq!(build_number("0.1.0").unwrap(), "0.1.900");
        assert_eq!(build_number("0.1.1-alpha.1").unwrap(), "0.1.1101");
        for bad in [
            "0.1",
            "0.1.0-dev.1",
            "0.1.0-alpha",
            "0.1.0-alpha.100",
            "x.1.0",
        ] {
            assert!(build_number(bad).is_err(), "{bad}");
        }
        // The version this workspace is at must work.
        build_number(env!("CARGO_PKG_VERSION")).unwrap();
    }

    #[test]
    fn finds_the_enclosing_bundle() {
        assert_eq!(enclosing_bundle(Path::new(BIN)), Some(PathBuf::from(APP)));
        assert_eq!(
            enclosing_bundle(Path::new("/x/target/release/aether-agent")),
            None
        );
        assert_eq!(
            enclosing_bundle(Path::new("/x/Foo/Contents/MacOS/aether-agent")),
            None
        );
    }

    #[test]
    fn homebrew_installs_are_recognised() {
        let cask = Path::new(
            "/opt/homebrew/Caskroom/aetherdesk-host/0.1.0/aetherdesk-host-0.1.0-arm64-macos/\
             AetherDesk.app/Contents/MacOS/aether-agent",
        );
        assert_eq!(install_source(cask), "homebrew");
        assert_eq!(
            install_source(Path::new("/tmp/AetherDesk.app/Contents/MacOS/aether-agent")),
            "manual"
        );
    }

    #[test]
    fn info_plist_has_the_updater_only_with_a_key() {
        let key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
        assert!(is_ed_public_key(key));
        assert!(!is_ed_public_key("REPLACE_ME"));
        let with = info_plist("0.1.0-alpha.4", FEED_URL, Some(key)).unwrap();
        assert!(with.contains("<string>0.1.104</string>"));
        assert!(with.contains("<string>0.1.0-alpha.4</string>"));
        assert!(with.contains(&format!("<string>{BUNDLE_ID}</string>")));
        assert!(with.contains("<key>SUPublicEDKey</key>"));
        assert!(with.contains("<key>SURequireSignedFeed</key>\n\t<true/>"));
        assert!(with.contains(FEED_URL));
        let without = info_plist("0.1.0-alpha.4", FEED_URL, None).unwrap();
        assert!(!without.contains("SUFeedURL"));
        assert!(!without.contains("SUPublicEDKey"));
        assert!(info_plist("0.1.0-alpha.4", FEED_URL, Some("REPLACE_ME")).is_err());
    }
}

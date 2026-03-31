use semver::Version;
use std::sync::OnceLock;
use tracing::error;

/// Vendor identifier embedded in build metadata so peers know this is the Irys implementation.
const VENDOR: &str = "irys-rs";

/// Global build version, initialized once.
///
/// Binary crates should call [`init_build_version`] early in `main()` to set the full version
/// including git metadata. Library crates access it via [`build_version`], which falls back to
/// the plain workspace version if no initializer ran.
static BUILD_VERSION: OnceLock<Version> = OnceLock::new();

/// Returns the build version. Always includes `+irys-rs` vendor tag in build metadata.
/// If [`init_build_version`] was called (by the binary), untagged commits also get the
/// git SHA appended: `3.0.0+irys-rs.abc1234`. Otherwise falls back to `3.0.0+irys-rs`.
pub fn build_version() -> Version {
    build_version_ref().clone()
}

fn build_version_ref() -> &'static Version {
    BUILD_VERSION.get_or_init(|| {
        let mut v =
            Version::parse(env!("CARGO_PKG_VERSION")).expect("valid CARGO_PKG_VERSION semver");
        v.build = semver::BuildMetadata::new(VENDOR).expect("valid build metadata");
        v
    })
}

/// Initializes the global build version with git metadata.
/// **Must be the first call in `main()`** — the backing `OnceLock` is set-once, so any
/// earlier call to [`build_version`] (e.g. via handshake `Default` impls) would permanently
/// freeze the version without git metadata.
///
/// - `pkg_version`: the caller's `env!("CARGO_PKG_VERSION")` — use the binary crate's version
/// - `git_sha`: 7-character short commit hash (e.g. `"a1b2c3d"`)
/// - `has_tag`: whether the commit has an exact tag match
/// - `is_dirty`: whether the working tree had uncommitted changes at build time
///
/// Examples:
/// - Tagged, clean:   `3.0.0+irys-rs`
/// - Untagged, clean: `3.0.0+irys-rs.a1b2c3d`
/// - Untagged, dirty: `3.0.0+irys-rs.a1b2c3d.dirty`
/// - Tagged, dirty:   `3.0.0+irys-rs.dirty`
///
/// No-op if already initialized.
pub fn init_build_version(pkg_version: &str, git_sha: &str, has_tag: bool, is_dirty: bool) {
    debug_assert!(
        has_tag || !git_sha.is_empty(),
        "untagged build must have a non-empty git_sha for version metadata"
    );
    let version = {
        let mut version = Version::parse(pkg_version).expect("valid pkg_version semver");
        let mut meta = if !has_tag && !git_sha.is_empty() {
            format!("{VENDOR}.{git_sha}")
        } else {
            VENDOR.to_string()
        };
        if is_dirty {
            meta.push_str(".dirty");
        }
        version.build = semver::BuildMetadata::new(&meta).expect("valid build metadata");
        version
    };
    if let Err(existing) = BUILD_VERSION.set(version) {
        debug_assert!(
            false,
            "init_build_version called after BUILD_VERSION already set to {existing}"
        );
        error!(
            existing = %existing,
            "init_build_version called too late — BUILD_VERSION already initialized, git metadata dropped"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_version_matches_cargo_pkg_version() {
        let v = build_version();
        let pkg = Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        assert_eq!(v.major, pkg.major);
        assert_eq!(v.minor, pkg.minor);
        assert_eq!(v.patch, pkg.patch);
        assert_eq!(v.pre, pkg.pre);
    }

    #[test]
    fn build_version_includes_vendor() {
        let v = build_version();
        assert!(
            v.build.as_str().starts_with("irys"),
            "build metadata should start with vendor: {}",
            v.build
        );
    }

    #[test]
    fn build_version_is_valid_semver_string() {
        let v = build_version();
        let reparsed = Version::parse(&v.to_string()).expect("build_version should roundtrip");
        assert_eq!(v, reparsed);
    }

    #[test]
    fn version_untagged_clean() {
        // Verify the construction logic used by init_build_version for untagged, clean commits.
        let mut v = Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        let sha = "abc1234";
        let meta = format!("{VENDOR}.{sha}");
        v.build = semver::BuildMetadata::new(&meta).unwrap();
        assert_eq!(
            v.to_string(),
            format!("{}+{}.{}", env!("CARGO_PKG_VERSION"), VENDOR, sha)
        );
    }

    #[test]
    fn version_tagged_clean() {
        let mut v = Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        v.build = semver::BuildMetadata::new(VENDOR).unwrap();
        assert_eq!(
            v.to_string(),
            format!("{}+{}", env!("CARGO_PKG_VERSION"), VENDOR)
        );
    }

    #[test]
    fn version_untagged_dirty() {
        let mut v = Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        let sha = "abc1234";
        let meta = format!("{VENDOR}.{sha}.dirty");
        v.build = semver::BuildMetadata::new(&meta).unwrap();
        assert_eq!(
            v.to_string(),
            format!("{}+{}.{}.dirty", env!("CARGO_PKG_VERSION"), VENDOR, sha)
        );
    }

    #[test]
    fn version_tagged_dirty() {
        let mut v = Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        let meta = format!("{VENDOR}.dirty");
        v.build = semver::BuildMetadata::new(&meta).unwrap();
        assert_eq!(
            v.to_string(),
            format!("{}+{}.dirty", env!("CARGO_PKG_VERSION"), VENDOR)
        );
    }

    // -- Golden-string fixture tests for serialization / deserialization --

    #[test]
    fn golden_tagged_release_serializes_to_json() {
        let mut v = Version::parse("3.0.0").unwrap();
        v.build = semver::BuildMetadata::new(VENDOR).unwrap();
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, r#""3.0.0+irys-rs""#);
    }

    #[test]
    fn golden_untagged_build_serializes_to_json() {
        let mut v = Version::parse("3.0.0").unwrap();
        v.build = semver::BuildMetadata::new(&format!("{VENDOR}.a1b2c3d")).unwrap();
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, r#""3.0.0+irys-rs.a1b2c3d""#);
    }

    #[test]
    fn golden_tagged_release_roundtrips_json() {
        let json = r#""3.0.0+irys-rs""#;
        let v: Version = serde_json::from_str(json).unwrap();
        assert_eq!(v.major, 3);
        assert_eq!(v.minor, 0);
        assert_eq!(v.patch, 0);
        assert_eq!(v.build.as_str(), "irys-rs");
        assert_eq!(serde_json::to_string(&v).unwrap(), json);
    }

    #[test]
    fn golden_untagged_build_roundtrips_json() {
        let json = r#""3.0.0+irys-rs.a1b2c3d""#;
        let v: Version = serde_json::from_str(json).unwrap();
        assert_eq!(v.major, 3);
        assert_eq!(v.minor, 0);
        assert_eq!(v.patch, 0);
        assert_eq!(v.build.as_str(), "irys-rs.a1b2c3d");
        assert_eq!(serde_json::to_string(&v).unwrap(), json);
    }

    #[test]
    fn golden_plain_version_without_metadata_deserializes() {
        // Peers running older versions may send plain "3.0.0" without vendor metadata.
        let json = r#""3.0.0""#;
        let v: Version = serde_json::from_str(json).unwrap();
        assert_eq!(v.major, 3);
        assert_eq!(v.minor, 0);
        assert_eq!(v.patch, 0);
        assert!(v.build.is_empty());
    }

    #[test]
    fn golden_foreign_vendor_deserializes() {
        // A hypothetical alternative implementation — we should accept it, not reject.
        let json = r#""3.0.0+irys-go.deadbeef""#;
        let v: Version = serde_json::from_str(json).unwrap();
        assert_eq!(v.major, 3);
        assert_eq!(v.build.as_str(), "irys-go.deadbeef");
    }

    #[test]
    fn golden_untagged_dirty_serializes_to_json() {
        let mut v = Version::parse("3.0.0").unwrap();
        v.build = semver::BuildMetadata::new(&format!("{VENDOR}.a1b2c3d.dirty")).unwrap();
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, r#""3.0.0+irys-rs.a1b2c3d.dirty""#);
    }

    #[test]
    fn golden_tagged_dirty_serializes_to_json() {
        let mut v = Version::parse("3.0.0").unwrap();
        v.build = semver::BuildMetadata::new(&format!("{VENDOR}.dirty")).unwrap();
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, r#""3.0.0+irys-rs.dirty""#);
    }

    #[test]
    fn golden_dirty_roundtrips_json() {
        let json = r#""3.0.0+irys-rs.a1b2c3d.dirty""#;
        let v: Version = serde_json::from_str(json).unwrap();
        assert_eq!(v.build.as_str(), "irys-rs.a1b2c3d.dirty");
        assert_eq!(serde_json::to_string(&v).unwrap(), json);
    }

    #[test]
    fn golden_build_metadata_ignored_for_precedence() {
        // SemVer 2.0.0: build metadata MUST be ignored when determining version precedence.
        // The semver crate's `cmp_precedence` method implements this correctly.
        let a: Version = serde_json::from_str(r#""3.0.0+irys-rs.abc""#).unwrap();
        let b: Version = serde_json::from_str(r#""3.0.0+irys-rs.def""#).unwrap();
        let c: Version = serde_json::from_str(r#""3.0.0""#).unwrap();
        assert_eq!(a.cmp_precedence(&b), std::cmp::Ordering::Equal);
        assert_eq!(a.cmp_precedence(&c), std::cmp::Ordering::Equal);
    }
}

use anyhow::{Context, Result};
use semver::{Version, VersionReq};

/// Validate an extension's `version`, naming the config file it came from.
///
/// The bare [`validate_semver`] message names the extension but not the file,
/// which is little help when the value was merged in from a remote extension's
/// own `avocado.yaml` several hops away.
pub fn validate_ext_version(ext_name: &str, version: &str, source_path: &str) -> Result<()> {
    validate_semver(version).with_context(|| {
        format!(
            "Extension '{ext_name}' has invalid version '{version}' (from {source_path}). \
             Version must be in semantic versioning format \
             (e.g., '1.0.0', '2.1.3', '1.0.0-rc.1', '1.0.0+build.5')"
        )
    })
}

/// Validate semantic versioning format (X.Y.Z where X, Y, Z are non-negative integers).
///
/// Accepts standard semver with optional pre-release and build metadata
/// (e.g., "1.0.0", "2024.0.0", "1.0.0-alpha", "1.0.0+build").
pub fn validate_semver(version: &str) -> Result<()> {
    let parts: Vec<&str> = version.split('.').collect();

    if parts.len() < 3 {
        return Err(anyhow::anyhow!(
            "Version must follow semantic versioning format with at least MAJOR.MINOR.PATCH components (e.g., '1.0.0', '2.1.3')"
        ));
    }

    // Validate the first 3 components (MAJOR.MINOR.PATCH)
    for (i, part) in parts.iter().take(3).enumerate() {
        // Handle pre-release and build metadata (e.g., "1.0.0-alpha" or "1.0.0+build")
        let component = part.split(&['-', '+'][..]).next().unwrap_or(part);

        component.parse::<u32>().with_context(|| {
            let component_name = match i {
                0 => "MAJOR",
                1 => "MINOR",
                2 => "PATCH",
                _ => "component",
            };
            format!(
                "{component_name} version component '{component}' must be a non-negative integer in semantic versioning format"
            )
        })?;
    }

    Ok(())
}

/// Check that the running CLI version satisfies a semver requirement string.
///
/// The requirement string uses standard semver requirement syntax (e.g., ">=0.25.0",
/// "^0.25", "~0.25.1", ">=0.25.0, <1.0.0").
pub fn check_cli_requirement(requirement: &str) -> Result<()> {
    let req = VersionReq::parse(requirement).with_context(|| {
        format!("Invalid cli_requirement '{requirement}'. Expected a semver requirement (e.g., '>=0.25.0', '^0.25')")
    })?;

    let running = Version::parse(env!("CARGO_PKG_VERSION")).with_context(|| {
        format!(
            "Failed to parse CLI version '{}' as semver",
            env!("CARGO_PKG_VERSION")
        )
    })?;

    // First try the exact running version, so a requirement that explicitly
    // pins a pre-release (e.g. "=1.0.0-rc.1") is still satisfiable when running
    // that build. Then fall back to the running version with pre-release/build
    // metadata stripped: semver only lets a pre-release satisfy a comparator
    // that carries a matching pre-release tag, so without this fallback an
    // ordinary requirement like ">=0.25" or "^1" would spuriously reject every
    // RC build. The full version is still shown in the error message below.
    let running_release = Version::new(running.major, running.minor, running.patch);

    if !req.matches(&running) && !req.matches(&running_release) {
        anyhow::bail!(
            "This project requires avocado CLI version '{requirement}', \
             but you are running version {running}.\n\n\
             Please update your avocado CLI."
        );
    }

    Ok(())
}

/// Convert a semver version string into an RPM-compatible `Version:` value.
///
/// RPM forbids `-` in the Version field (it is the Version/Release separator),
/// so a semver pre-release like `1.0.0-rc.1` is illegal and `rpmbuild` rejects
/// it. RPM uses `~` for pre-release ordering — `1.0.0~rc.1` sorts *before*
/// `1.0.0`, matching semver pre-release precedence — and `^` for post-release,
/// so map the pre-release `-` to `~` and the build-metadata `+` to `^`. A plain
/// release version (no `-`/`+`) is returned unchanged.
///
/// The mapping is exact and reversible: semver's grammar admits only
/// `[0-9A-Za-z.+-]`, so no `~` or `^` can occur in the input and
/// [`from_rpm_version`] recovers the original string byte for byte.
///
/// A `-` *inside* a pre-release or build identifier (`1.0.0-rc-1`, legal semver)
/// is **rejected** rather than rewritten. Leaving it is not an option — RPM
/// forbids `-` anywhere in `Version:`, so `1.0.0~rc-1` is refused outright — but
/// mapping it to `~` as well would silently invert ordering, because `~` is a
/// precedence operator to RPM and an ordinary character to semver:
///
/// | semver                        | RPM after mapping             |
/// |-------------------------------|-------------------------------|
/// | `1.0.0-rc1 < 1.0.0-rc1-fix`   | `1.0.0~rc1` **>** `1.0.0~rc1~fix` |
/// | `1.0.0-rc-1 > 1.0.0-rc.2`     | `1.0.0~rc~1` **<** `1.0.0~rc.2`   |
///
/// dnf would treat the newer version as the older one and refuse the upgrade. No
/// substitute character rescues it either: `_` makes `1.0.0~rc_1` and
/// `1.0.0~rc.1` compare equal, and `.` collides outright. Refusing the input is
/// the only behavior that can't ship a wrong answer, and it costs only version
/// strings nobody writes — this project ships `X.Y.Z` and `X.Y.Z-rc.N`.
pub fn to_rpm_version(version: &str) -> Result<String> {
    // Build metadata is whatever follows the first `+`; the pre-release is
    // whatever follows the first `-` before it. A second `-` in either is inside
    // an identifier rather than a separator.
    let (core_pre, build) = match version.split_once('+') {
        Some((core_pre, build)) => (core_pre, Some(build)),
        None => (version, None),
    };
    let hyphen_in_identifier = core_pre
        .split_once('-')
        .is_some_and(|(_core, pre)| pre.contains('-'))
        || build.is_some_and(|b| b.contains('-'));

    if hyphen_in_identifier {
        anyhow::bail!(
            "Version '{version}' has a '-' inside a pre-release or build identifier. \
             RPM forbids '-' in `Version:`, and rewriting it to '~' would invert version \
             ordering — dnf would see the newer version as the older one and refuse the \
             upgrade. Use '.' to separate identifiers instead (e.g. '1.0.0-rc.1')."
        );
    }

    Ok(version.replace('-', "~").replace('+', "^"))
}

/// Invert [`to_rpm_version`], recovering the semver string from an RPM
/// `Version:` value. Exact for anything `to_rpm_version` produced.
///
/// Everything in avocado outside the RPM spec and the NVR filename speaks
/// semver — config, the payload's `avocado.yaml`, the version the platform
/// records — so a version read back *out* of rpm (an rpmdb query, a parsed NVR)
/// has to come back through here or it won't match the config it came from.
pub fn from_rpm_version(rpm_version: &str) -> String {
    rpm_version.replace('~', "-").replace('^', "+")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_semver() {
        assert!(validate_semver("1.0.0").is_ok());
        assert!(validate_semver("2024.0.0").is_ok());
        assert!(validate_semver("0.1.0").is_ok());
        assert!(validate_semver("1.0.0-alpha").is_ok());
        assert!(validate_semver("1.0.0+build").is_ok());
        assert!(validate_semver("1.0.0.1").is_ok()); // extra components allowed
    }

    #[test]
    fn test_invalid_semver() {
        assert!(validate_semver("1.0").is_err());
        assert!(validate_semver("1").is_err());
        assert!(validate_semver("*").is_err());
        assert!(validate_semver("2024.*").is_err());
        assert!(validate_semver("abc.def.ghi").is_err());
    }

    #[test]
    fn test_check_cli_requirement_satisfied() {
        // Any released version is >= 0.0.1. This also covers pre-release builds
        // (e.g. `1.0.0-rc.0`), which are matched as their release version.
        assert!(check_cli_requirement(">=0.0.1").is_ok());
        // Exact current version
        let current = env!("CARGO_PKG_VERSION");
        assert!(check_cli_requirement(&format!(">={current}")).is_ok());
        // A requirement that explicitly pins the exact running version —
        // including a pre-release tag — must still match that build.
        assert!(check_cli_requirement(&format!("={current}")).is_ok());
    }

    #[test]
    fn test_check_cli_requirement_not_satisfied() {
        let result = check_cli_requirement(">=999.0.0");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains(">=999.0.0"));
        assert!(msg.contains(env!("CARGO_PKG_VERSION")));
        assert!(msg.contains("update"));
    }

    #[test]
    fn test_check_cli_requirement_complex() {
        // Caret requirement on the running major should match (derived so the
        // test doesn't rot across major bumps).
        let major = Version::parse(env!("CARGO_PKG_VERSION")).unwrap().major;
        assert!(check_cli_requirement(&format!("^{major}")).is_ok());
        // Wildcard that matches anything
        assert!(check_cli_requirement("*").is_ok());
    }

    #[test]
    fn test_to_rpm_version() {
        // Plain release versions are unchanged.
        assert_eq!(to_rpm_version("1.0.0").unwrap(), "1.0.0");
        assert_eq!(to_rpm_version("2.1.3").unwrap(), "2.1.3");
        // Pre-release `-` becomes `~` (sorts before the release in RPM).
        assert_eq!(to_rpm_version("1.0.0-rc.1").unwrap(), "1.0.0~rc.1");
        assert_eq!(to_rpm_version("1.0.0-alpha.2").unwrap(), "1.0.0~alpha.2");
        // Build metadata `+` becomes `^`.
        assert_eq!(to_rpm_version("1.0.0+build.5").unwrap(), "1.0.0^build.5");
    }

    #[test]
    fn test_to_rpm_version_handles_prerelease_and_build_together() {
        // Both separators present: `-` -> `~` and `+` -> `^` in one pass, with
        // the pre-release still ordering before the release.
        assert_eq!(
            to_rpm_version("1.0.0-rc.1+build.5").unwrap(),
            "1.0.0~rc.1^build.5"
        );
        // The real shipping shape for this project.
        assert_eq!(to_rpm_version("1.0.0-rc.1").unwrap(), "1.0.0~rc.1");
    }

    /// A `-` inside an identifier can't be rewritten to `~` — `~` is a
    /// precedence operator in RPM, so `1.0.0~rc1~fix` sorts *below* `1.0.0~rc1`
    /// while semver puts `1.0.0-rc1-fix` above `1.0.0-rc1`. dnf would refuse the
    /// upgrade. It can't be left alone either (rpmbuild rejects any `-`), so the
    /// only answer that isn't silently wrong is to refuse the input.
    #[test]
    fn test_to_rpm_version_rejects_hyphen_inside_identifier() {
        for v in [
            "1.0.0-rc-1",          // hyphen in a pre-release identifier
            "1.0.0-pre-release-2", // several
            "1.0.0+build-5",       // hyphen in build metadata
            "1.0.0-rc.1+build-5",  // valid pre-release, bad build metadata
            "2.0.0-a-b-c+d-e-f",   // bad in both
            "1.0.0-rc1-fix",       // the measured ordering inversion
        ] {
            let err = to_rpm_version(v).expect_err("{v} should be rejected");
            let msg = err.to_string();
            assert!(msg.contains(v), "error should name the version: {msg}");
            assert!(
                msg.contains("invert version ordering"),
                "error should explain why, got: {msg}"
            );
        }
    }

    /// The property the RPM spec actually depends on: whatever comes back out of
    /// `to_rpm_version` carries no `-`, because a surviving hyphen is a spec
    /// `rpmbuild` refuses outright.
    #[test]
    fn test_to_rpm_version_never_emits_a_hyphen() {
        for v in ["1.0.0", "1.0.0-rc.1", "1.0.0+build.5", "1.0.0-rc.1+build.5"] {
            let mapped = to_rpm_version(v).unwrap();
            assert!(!mapped.contains('-'), "{v} -> {mapped} still has a hyphen");
        }
    }

    /// The mapping is injective, so it inverts exactly — which is what the
    /// publish and rpmdb-query paths rely on to get back to the config's semver.
    #[test]
    fn test_rpm_version_round_trips() {
        for v in [
            "1.0.0",
            "2.1.3",
            "1.0.0-rc.1",
            "1.0.0-alpha.2",
            "1.0.0+build.5",
            "1.0.0-rc.1+build.5",
        ] {
            let rpm = to_rpm_version(v).unwrap();
            assert_eq!(
                from_rpm_version(&rpm),
                v,
                "{v} did not round-trip via {rpm}"
            );
        }
        // A release version has nothing to invert.
        assert_eq!(from_rpm_version("1.0.0"), "1.0.0");
    }

    #[test]
    fn test_check_cli_requirement_invalid_syntax() {
        let result = check_cli_requirement("not-a-requirement");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Invalid cli_requirement"));
    }
}

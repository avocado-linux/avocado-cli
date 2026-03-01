use anyhow::{Context, Result};
use semver::{Version, VersionReq};

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

    if !req.matches(&running) {
        anyhow::bail!(
            "This project requires avocado CLI version '{requirement}', \
             but you are running version {running}.\n\n\
             Please update your avocado CLI."
        );
    }

    Ok(())
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
        // Current version is 0.25.1, so >=0.0.1 should pass
        assert!(check_cli_requirement(">=0.0.1").is_ok());
        // Exact current version
        let current = env!("CARGO_PKG_VERSION");
        assert!(check_cli_requirement(&format!(">={current}")).is_ok());
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
        // Caret requirement that should match
        assert!(check_cli_requirement("^0").is_ok());
        // Wildcard that matches anything
        assert!(check_cli_requirement("*").is_ok());
    }

    #[test]
    fn test_check_cli_requirement_invalid_syntax() {
        let result = check_cli_requirement("not-a-requirement");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Invalid cli_requirement"));
    }
}

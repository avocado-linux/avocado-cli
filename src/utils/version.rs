use anyhow::{Context, Result};

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
}

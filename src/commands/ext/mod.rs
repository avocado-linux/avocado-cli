pub mod build;
pub mod checkout;
pub mod clean;
pub mod deps;
pub mod dnf;
pub mod fetch;
pub mod image;
pub mod install;
pub mod list;
pub mod package;

use crate::utils::interpolation::interpolate_name;

pub use build::ExtBuildCommand;
#[allow(unused_imports)]
pub use checkout::ExtCheckoutCommand;
#[allow(unused_imports)]
pub use clean::ExtCleanCommand;
#[allow(unused_imports)]
pub use deps::ExtDepsCommand;
#[allow(unused_imports)]
pub use dnf::ExtDnfCommand;
pub use fetch::ExtFetchCommand;
pub use image::ExtImageCommand;
pub use install::ExtInstallCommand;
#[allow(unused_imports)]
pub use list::ExtListCommand;
#[allow(unused_imports)]
pub use package::ExtPackageCommand;

/// Look up an extension's config value from the composed config's extensions section.
///
/// Handles template keys like `avocado-bsp-{{ avocado.target }}` by first trying a direct
/// lookup, then iterating through all extension keys and matching via interpolation.
pub(crate) fn find_ext_in_mapping<'a>(
    parsed: &'a serde_yaml::Value,
    extension_name: &str,
    target: &str,
) -> Option<&'a serde_yaml::Value> {
    let ext_section = parsed.get("extensions")?;

    // Try direct lookup first (works when keys are already interpolated)
    if let Some(val) = ext_section.get(extension_name) {
        return Some(val);
    }

    // Fall back to iterating and matching template keys
    let ext_map = ext_section.as_mapping()?;
    for (key, value) in ext_map {
        if let Some(key_str) = key.as_str() {
            if key_str.contains("{{") {
                let interpolated = interpolate_name(key_str, target);
                if interpolated == extension_name {
                    return Some(value);
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(yaml: &str) -> serde_yaml::Value {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn test_find_ext_direct_lookup() {
        let config = make_config(
            r#"
extensions:
  avocado-bsp-raspberrypi5:
    version: "1.0.0"
    source:
      type: package
"#,
        );
        let result = find_ext_in_mapping(&config, "avocado-bsp-raspberrypi5", "raspberrypi5");
        assert!(result.is_some());
        assert_eq!(result.unwrap().get("version").unwrap().as_str(), Some("1.0.0"));
    }

    #[test]
    fn test_find_ext_template_key_fallback() {
        let config = make_config(
            r#"
extensions:
  avocado-bsp-{{ avocado.target }}:
    version: "2.0.0"
    source:
      type: package
"#,
        );
        // Direct lookup for the interpolated name should fail, but template matching succeeds
        let result = find_ext_in_mapping(&config, "avocado-bsp-raspberrypi5", "raspberrypi5");
        assert!(result.is_some());
        assert_eq!(result.unwrap().get("version").unwrap().as_str(), Some("2.0.0"));
    }

    #[test]
    fn test_find_ext_template_key_different_target() {
        let config = make_config(
            r#"
extensions:
  avocado-bsp-{{ avocado.target }}:
    version: "3.0.0"
"#,
        );
        let result = find_ext_in_mapping(&config, "avocado-bsp-jetson-orin-nano", "jetson-orin-nano");
        assert!(result.is_some());
        assert_eq!(result.unwrap().get("version").unwrap().as_str(), Some("3.0.0"));
    }

    #[test]
    fn test_find_ext_not_found() {
        let config = make_config(
            r#"
extensions:
  some-other-ext:
    version: "1.0.0"
"#,
        );
        let result = find_ext_in_mapping(&config, "avocado-bsp-raspberrypi5", "raspberrypi5");
        assert!(result.is_none());
    }

    #[test]
    fn test_find_ext_no_extensions_section() {
        let config = make_config("sdk:\n  image: test\n");
        let result = find_ext_in_mapping(&config, "avocado-bsp-raspberrypi5", "raspberrypi5");
        assert!(result.is_none());
    }

    #[test]
    fn test_find_ext_template_no_spacing() {
        let config = make_config(
            r#"
extensions:
  avocado-bsp-{{avocado.target}}:
    version: "4.0.0"
"#,
        );
        let result = find_ext_in_mapping(&config, "avocado-bsp-raspberrypi5", "raspberrypi5");
        assert!(result.is_some());
        assert_eq!(result.unwrap().get("version").unwrap().as_str(), Some("4.0.0"));
    }
}

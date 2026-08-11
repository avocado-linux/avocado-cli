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

/// Resolve a remote / path-sourced extension's effective config from the
/// composed value, applying its `target-<name>:` (and legacy bare `<name>:`)
/// per-target overrides. Any `kernel-<spec>:` override keys are stripped, not
/// applied — the kernel version isn't known at ext build/image time, so we pass
/// `resolved_kver: None` (the same as `get_merged_ext_config`, keeping the
/// `Local` and remote paths consistent).
///
/// Shared by `ext build` and `ext image`. The composed config holds the base
/// extension keys plus its `target-<name>:` sub-sections (as
/// `merge_installed_remote_extensions` produces for a path/remote source);
/// running it through `resolve_overrides_in_value` makes the runtime-build path
/// honor the same overrides the standalone `Local` path gets via
/// `get_merged_ext_config` — otherwise only a bare `<target>:` key matched and
/// the preferred `target-<name>:` form (overlay + `--tag`) was silently ignored.
/// Returns `None` when the extension isn't present in the composed config.
pub(crate) fn resolve_remote_ext_config(
    config: &crate::utils::config::Config,
    parsed: &serde_yaml::Value,
    extension_name: &str,
    target: &str,
) -> Option<serde_yaml::Value> {
    let ext_val = find_ext_in_mapping(parsed, extension_name, target)?;
    Some(config.resolve_overrides_in_value(
        ext_val.clone(),
        target,
        None,
        &format!("extensions.{extension_name}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(yaml: &str) -> serde_yaml::Value {
        serde_yaml::from_str(yaml).unwrap()
    }

    /// Regression: a remote / path-sourced extension in a runtime build has the
    /// base keys plus `target-<name>:` sub-sections in the composed value. The
    /// shared resolver (used by `ext build` + `ext image`) must honor the
    /// preferred `target-<name>:` overlay + `--tag`, not just a bare `<target>:`
    /// key, and must strip the override sub-keys rather than leak them.
    #[test]
    fn test_resolve_remote_ext_config_target_prefix_override() {
        let yaml = r#"
supported_targets:
  - raspberrypi4
  - qemux86-64
sdk:
  image: "docker.io/avocadolinux/sdk:apollo-edge"
extensions:
  kos-layer-boardconf:
    version: 2026.7.0
    image:
      type: kab
      args: '-b -t kos.layer -v 2026.7.0 --tag {{ avocado.target }}'
    target-raspberrypi4:
      overlay: overlay/raspberrypi4
    target-qemux86-64:
      overlay: overlay/qemu-x64
      image:
        args: '-b -t kos.layer -v 2026.7.0 --tag qemu-x64'
"#;
        let config = crate::utils::config::Config::load_from_yaml_str(yaml).unwrap();
        let parsed = make_config(yaml);

        // qemux86-64: target overlay + tag win, base image.type kept, overrides stripped.
        let q = resolve_remote_ext_config(&config, &parsed, "kos-layer-boardconf", "qemux86-64")
            .expect("extension present");
        assert_eq!(
            q.get("overlay").and_then(|v| v.as_str()),
            Some("overlay/qemu-x64")
        );
        let q_args = q
            .get("image")
            .and_then(|i| i.get("args"))
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(q_args.contains("--tag qemu-x64"), "got: {q_args}");
        assert_eq!(
            q.get("image")
                .and_then(|i| i.get("type"))
                .and_then(|v| v.as_str()),
            Some("kab")
        );
        assert!(q.get("target-qemux86-64").is_none());
        assert!(q.get("target-raspberrypi4").is_none());

        // raspberrypi4: its overlay wins (no image override → base args remain).
        let r = resolve_remote_ext_config(&config, &parsed, "kos-layer-boardconf", "raspberrypi4")
            .expect("extension present");
        assert_eq!(
            r.get("overlay").and_then(|v| v.as_str()),
            Some("overlay/raspberrypi4")
        );

        // A target with no matching override gets the base only — no overlay
        // leaks from a sibling target-* section.
        let o = resolve_remote_ext_config(&config, &parsed, "kos-layer-boardconf", "qemuarm64")
            .expect("extension present");
        assert!(o.get("overlay").is_none());
        assert!(o.get("target-qemux86-64").is_none());

        // Absent extension → None.
        assert!(
            resolve_remote_ext_config(&config, &parsed, "does-not-exist", "qemux86-64").is_none()
        );
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
        assert_eq!(
            result.unwrap().get("version").unwrap().as_str(),
            Some("1.0.0")
        );
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
        assert_eq!(
            result.unwrap().get("version").unwrap().as_str(),
            Some("2.0.0")
        );
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
        let result =
            find_ext_in_mapping(&config, "avocado-bsp-jetson-orin-nano", "jetson-orin-nano");
        assert!(result.is_some());
        assert_eq!(
            result.unwrap().get("version").unwrap().as_str(),
            Some("3.0.0")
        );
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
        assert_eq!(
            result.unwrap().get("version").unwrap().as_str(),
            Some("4.0.0")
        );
    }
}

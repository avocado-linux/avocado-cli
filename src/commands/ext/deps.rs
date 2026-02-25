use anyhow::Result;
use std::collections::HashSet;
use std::sync::Arc;

use crate::utils::config::{ComposedConfig, Config};
use crate::utils::output::{print_error, OutputLevel};
use crate::utils::target::resolve_target_required;

pub struct ExtDepsCommand {
    config_path: String,
    extension: Option<String>,
    target: Option<String>,
    /// Pre-composed configuration to avoid reloading
    composed_config: Option<Arc<ComposedConfig>>,
}

impl ExtDepsCommand {
    pub fn new(config_path: String, extension: Option<String>, target: Option<String>) -> Self {
        Self {
            config_path,
            extension,
            target,
            composed_config: None,
        }
    }

    /// Set pre-composed configuration to avoid reloading
    #[allow(dead_code)]
    pub fn with_composed_config(mut self, config: Arc<ComposedConfig>) -> Self {
        self.composed_config = Some(config);
        self
    }

    pub fn execute(&self) -> Result<()> {
        // Use provided config or load fresh
        let composed = match &self.composed_config {
            Some(cc) => Arc::clone(cc),
            None => Arc::new(Config::load_composed(
                &self.config_path,
                self.target.as_deref(),
            )?),
        };
        let config = &composed.config;
        let parsed = &composed.merged_value;

        let target = resolve_target_required(self.target.as_deref(), config)?;
        let extensions_to_process = self.get_extensions_to_process(config, parsed, &target)?;

        self.display_dependencies(parsed, &extensions_to_process);
        Ok(())
    }

    fn get_extensions_to_process(
        &self,
        config: &Config,
        parsed: &serde_yaml::Value,
        target: &str,
    ) -> Result<Vec<String>> {
        match &self.extension {
            Some(extension_name) => {
                // Use comprehensive lookup for specific extension
                match config.find_extension_in_dependency_tree(
                    &self.config_path,
                    extension_name,
                    target,
                )? {
                    Some(_location) => Ok(vec![extension_name.clone()]),
                    None => {
                        self.print_extension_not_found(extension_name);
                        Err(anyhow::anyhow!("Extension not found"))
                    }
                }
            }
            None => {
                // For listing all extensions, still use local extensions only
                let ext_section = parsed.get("extensions");
                match ext_section {
                    Some(ext) => {
                        let ext_table = ext
                            .as_mapping()
                            .ok_or_else(|| anyhow::anyhow!("Invalid ext section format"))?;
                        Ok(ext_table
                            .keys()
                            .filter_map(|k| k.as_str().map(|s| s.to_string()))
                            .collect())
                    }
                    None => {
                        self.handle_no_extensions();
                        Err(anyhow::anyhow!("No ext section found"))
                    }
                }
            }
        }
    }

    fn handle_no_extensions(&self) {
        match &self.extension {
            Some(extension_name) => {
                print_error(
                    &format!("Extension '{extension_name}' not found in configuration."),
                    OutputLevel::Normal,
                );
            }
            None => {
                println!("No extensions found in configuration.");
            }
        }
    }

    fn print_extension_not_found(&self, extension_name: &str) {
        print_error(
            &format!("Extension '{extension_name}' not found in configuration."),
            OutputLevel::Normal,
        );
    }

    fn display_dependencies(&self, parsed: &serde_yaml::Value, extensions: &[String]) {
        if extensions.is_empty() {
            println!("No extensions found in configuration.");
            return;
        }

        for ext_name in extensions {
            println!("Extension: {ext_name}");

            let dependencies = self.list_packages_from_config(parsed, ext_name);
            self.print_dependencies(&dependencies);
            println!();
        }
    }

    fn print_dependencies(&self, dependencies: &[(String, String, String)]) {
        if dependencies.is_empty() {
            println!("  No dependencies");
            return;
        }

        for (dep_type, pkg_name, pkg_version) in dependencies {
            let type_prefix = if dep_type == "ext" { "ext:" } else { "pkg:" };
            println!("  {type_prefix}{pkg_name} = {pkg_version}");
        }
    }

    fn resolve_package_dependencies(
        &self,
        config: &serde_yaml::Value,
        package_name: &str,
        package_spec: &serde_yaml::Value,
    ) -> Vec<(String, String, String)> {
        match package_spec {
            serde_yaml::Value::String(version) => {
                vec![("pkg".to_string(), package_name.to_string(), version.clone())]
            }
            serde_yaml::Value::Mapping(spec_map) => {
                self.resolve_table_dependency(config, package_name, spec_map)
            }
            _ => Vec::new(),
        }
    }

    fn resolve_table_dependency(
        &self,
        config: &serde_yaml::Value,
        package_name: &str,
        spec_map: &serde_yaml::Mapping,
    ) -> Vec<(String, String, String)> {
        // Try version first
        if let Some(serde_yaml::Value::String(version)) = spec_map.get("version") {
            return vec![("pkg".to_string(), package_name.to_string(), version.clone())];
        }

        // Try extension reference
        if let Some(serde_yaml::Value::String(ext_name)) = spec_map.get("extensions") {
            // Check if this is a versioned extension (has vsn field)
            if let Some(serde_yaml::Value::String(version)) = spec_map.get("vsn") {
                return vec![("ext".to_string(), ext_name.clone(), version.clone())];
            }
            // Check if this is an external extension (has config field)
            else if spec_map.get("config").is_some() {
                // For external extensions, we don't have a local version, so use "*"
                return vec![("ext".to_string(), ext_name.clone(), "*".to_string())];
            } else {
                // Local extension - resolve from local config
                return self.resolve_extension_dependency(config, ext_name);
            }
        }

        // Try compile reference (both old and new syntax)
        if let Some(serde_yaml::Value::String(compile_name)) = spec_map.get("compile") {
            // Check if this is the new syntax with install script
            if let Some(serde_yaml::Value::String(install_script)) = spec_map.get("install") {
                // New syntax: { compile = "section-name", install = "script.sh" }
                // Return a special marker to indicate this needs install script handling
                return vec![(
                    "compile_with_install".to_string(),
                    compile_name.clone(),
                    install_script.clone(),
                )];
            } else {
                // Old syntax: { compile = "section-name" }
                return self.resolve_compile_dependencies(config, compile_name);
            }
        }

        Vec::new()
    }

    fn resolve_extension_dependency(
        &self,
        config: &serde_yaml::Value,
        ext_name: &str,
    ) -> Vec<(String, String, String)> {
        let version = config
            .get("extensions")
            .and_then(|ext_section| ext_section.get(ext_name))
            .and_then(|ext_config| ext_config.get("version"))
            .and_then(|v| v.as_str())
            .unwrap_or("*");

        vec![("ext".to_string(), ext_name.to_string(), version.to_string())]
    }

    fn resolve_compile_dependencies(
        &self,
        config: &serde_yaml::Value,
        compile_name: &str,
    ) -> Vec<(String, String, String)> {
        let compile_deps = config
            .get("sdk")
            .and_then(|sdk| sdk.get("compile"))
            .and_then(|compile| compile.get(compile_name))
            .and_then(|compile_config| compile_config.get("packages"))
            .and_then(|deps| deps.as_mapping());

        let Some(deps_table) = compile_deps else {
            return Vec::new();
        };

        deps_table
            .iter()
            .filter_map(|(dep_name_val, dep_spec)| {
                dep_name_val
                    .as_str()
                    .and_then(|dep_name| self.extract_dependency_version(dep_name, dep_spec))
            })
            .collect()
    }

    fn extract_dependency_version(
        &self,
        dep_name: &str,
        dep_spec: &serde_yaml::Value,
    ) -> Option<(String, String, String)> {
        match dep_spec {
            serde_yaml::Value::String(version) => {
                Some(("pkg".to_string(), dep_name.to_string(), version.clone()))
            }
            serde_yaml::Value::Mapping(spec_map) => spec_map
                .get("version")
                .and_then(|v| v.as_str())
                .map(|version| ("pkg".to_string(), dep_name.to_string(), version.to_string())),
            _ => None,
        }
    }

    fn list_packages_from_config(
        &self,
        config: &serde_yaml::Value,
        extension: &str,
    ) -> Vec<(String, String, String)> {
        let dependencies = config
            .get("extensions")
            .and_then(|ext_section| ext_section.get(extension))
            .and_then(|ext_config| ext_config.get("packages"))
            .and_then(|deps| deps.as_mapping());

        let Some(deps_table) = dependencies else {
            return Vec::new();
        };

        let mut all_packages: Vec<_> = deps_table
            .iter()
            .flat_map(|(package_name_val, package_spec)| {
                package_name_val
                    .as_str()
                    .map(|package_name| {
                        self.resolve_package_dependencies(config, package_name, package_spec)
                    })
                    .unwrap_or_default()
            })
            .collect();

        self.deduplicate_and_sort(&mut all_packages);
        all_packages
    }

    fn deduplicate_and_sort(&self, packages: &mut Vec<(String, String, String)>) {
        // Remove duplicates while preserving order
        let mut seen = HashSet::new();
        packages.retain(|pkg| seen.insert(pkg.clone()));

        // Sort: extensions first, then packages, both alphabetically
        packages.sort_by(|a, b| {
            let a_is_ext = a.0 == "ext";
            let b_is_ext = b.0 == "ext";
            match (a_is_ext, b_is_ext) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.1.cmp(&b.1),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_compile_dependency_with_install() {
        let config_content = r#"
extensions:
  my-extension:
    types:
      - sysext
    packages:
      my-app:
        compile: my-app
        install: ext-install.sh
      regular-package: "1.0.0"
      old-compile-dep:
        compile: old-section

sdk:
  compile:
    my-app:
      compile: ext-compile.sh
    old-section:
      compile: ext-compile.sh
"#;

        let config: serde_yaml::Value = serde_yaml::from_str(config_content).unwrap();
        let cmd = ExtDepsCommand {
            config_path: "test.yaml".to_string(),
            extension: Some("my-extension".to_string()),
            target: None,
            composed_config: None,
        };

        // Test new syntax with install script
        let spec_map = serde_yaml::Mapping::from_iter([
            (
                serde_yaml::Value::String("compile".to_string()),
                serde_yaml::Value::String("my-app".to_string()),
            ),
            (
                serde_yaml::Value::String("install".to_string()),
                serde_yaml::Value::String("ext-install.sh".to_string()),
            ),
        ]);

        let result = cmd.resolve_table_dependency(&config, "my-app", &spec_map);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "compile_with_install");
        assert_eq!(result[0].1, "my-app");
        assert_eq!(result[0].2, "ext-install.sh");

        // Test old syntax without install script
        let spec_map_old = serde_yaml::Mapping::from_iter([(
            serde_yaml::Value::String("compile".to_string()),
            serde_yaml::Value::String("old-section".to_string()),
        )]);

        let result_old = cmd.resolve_table_dependency(&config, "old-compile-dep", &spec_map_old);
        // Should resolve to compile dependencies (empty in this test case since no deps in old-section)
        assert_eq!(result_old.len(), 0);
    }

    #[test]
    fn test_resolve_regular_dependencies() {
        let config_content = r#"
extensions:
  test-ext:
    types:
      - sysext
"#;

        let config: serde_yaml::Value = serde_yaml::from_str(config_content).unwrap();
        let cmd = ExtDepsCommand {
            config_path: "test.yaml".to_string(),
            extension: Some("test-ext".to_string()),
            target: None,
            composed_config: None,
        };

        // Test version dependency
        let spec_map = serde_yaml::Mapping::from_iter([(
            serde_yaml::Value::String("version".to_string()),
            serde_yaml::Value::String("1.0.0".to_string()),
        )]);

        let result = cmd.resolve_table_dependency(&config, "test-package", &spec_map);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "pkg");
        assert_eq!(result[0].1, "test-package");
        assert_eq!(result[0].2, "1.0.0");
    }
}

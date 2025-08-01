use anyhow::Result;
use std::collections::HashSet;

use crate::utils::config::load_config;
use crate::utils::output::{print_error, OutputLevel};

pub struct ExtDepsCommand {
    config_path: String,
    extension: Option<String>,
}

impl ExtDepsCommand {
    pub fn new(config_path: String, extension: Option<String>) -> Self {
        Self {
            config_path,
            extension,
        }
    }

    pub fn execute(&self) -> Result<()> {
        // Load configuration and parse raw TOML
        let _config = load_config(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;

        // Check if ext section exists
        let ext_section = match parsed.get("ext") {
            Some(ext) => ext,
            None => {
                if let Some(extension_name) = &self.extension {
                    print_error(
                        &format!("Extension '{}' not found in configuration.", extension_name),
                        OutputLevel::Normal,
                    );
                    return Ok(());
                } else {
                    println!("No extensions found in configuration.");
                    return Ok(());
                }
            }
        };

        // Determine which extensions to show dependencies for
        let extensions_to_process = if let Some(extension_name) = &self.extension {
            // Single extension specified
            if !ext_section.as_table().unwrap().contains_key(extension_name) {
                print_error(
                    &format!("Extension '{}' not found in configuration.", extension_name),
                    OutputLevel::Normal,
                );
                return Ok(());
            }
            vec![extension_name.clone()]
        } else {
            // No extension specified - show all extensions
            match ext_section.as_table() {
                Some(table) => table.keys().cloned().collect(),
                None => vec![],
            }
        };

        if extensions_to_process.is_empty() {
            println!("No extensions found in configuration.");
            return Ok(());
        }

        // Show dependencies for each extension
        for ext_name in &extensions_to_process {
            println!("Extension: {}", ext_name);

            let dependencies = self.list_packages_from_config(&parsed, ext_name);

            if dependencies.is_empty() {
                println!("  No dependencies");
            } else {
                for (dep_type, pkg_name, pkg_version) in dependencies {
                    let type_prefix = if dep_type == "ext" { "ext:" } else { "pkg:" };
                    println!("  {}{} = {}", type_prefix, pkg_name, pkg_version);
                }
            }
            println!();
        }

        Ok(())
    }

    fn resolve_package_dependencies(
        &self,
        config: &toml::Value,
        package_name: &str,
        package_spec: &toml::Value,
    ) -> Vec<(String, String, String)> {
        let mut dependencies = Vec::new();

        match package_spec {
            toml::Value::String(version) => {
                // Simple string version: "package-name = version"
                dependencies.push(("pkg".to_string(), package_name.to_string(), version.clone()));
            }
            toml::Value::Table(spec_map) => {
                if let Some(version) = spec_map.get("version") {
                    if let toml::Value::String(version_str) = version {
                        // Object with version: "package-name = { version = "1.0.0" }"
                        dependencies.push((
                            "pkg".to_string(),
                            package_name.to_string(),
                            version_str.clone(),
                        ));
                    }
                } else if let Some(ext_ref) = spec_map.get("ext") {
                    if let toml::Value::String(ext_name) = ext_ref {
                        // Extension reference
                        let mut version = "*".to_string();
                        if let Some(ext_section) = config.get("ext") {
                            if let Some(ext_config) = ext_section.get(ext_name) {
                                if let Some(ext_version) = ext_config.get("version") {
                                    if let toml::Value::String(version_str) = ext_version {
                                        version = version_str.clone();
                                    }
                                }
                            }
                        }
                        dependencies.push(("ext".to_string(), ext_name.clone(), version));
                    }
                } else if let Some(compile_ref) = spec_map.get("compile") {
                    if let toml::Value::String(compile_name) = compile_ref {
                        // Object with compile reference - only list the compile dependencies
                        if let Some(sdk_section) = config.get("sdk") {
                            if let Some(compile_section) = sdk_section.get("compile") {
                                if let Some(compile_config) = compile_section.get(compile_name) {
                                    if let Some(compile_deps) = compile_config.get("dependencies") {
                                        if let toml::Value::Table(deps_map) = compile_deps {
                                            for (dep_name, dep_spec) in deps_map {
                                                match dep_spec {
                                                    toml::Value::String(dep_version) => {
                                                        dependencies.push((
                                                            "pkg".to_string(),
                                                            dep_name.clone(),
                                                            dep_version.clone(),
                                                        ));
                                                    }
                                                    toml::Value::Table(dep_spec_map) => {
                                                        if let Some(dep_version) =
                                                            dep_spec_map.get("version")
                                                        {
                                                            if let toml::Value::String(
                                                                version_str,
                                                            ) = dep_version
                                                            {
                                                                dependencies.push((
                                                                    "pkg".to_string(),
                                                                    dep_name.clone(),
                                                                    version_str.clone(),
                                                                ));
                                                            }
                                                        }
                                                    }
                                                    _ => {}
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        dependencies
    }

    fn list_packages_from_config(
        &self,
        config: &toml::Value,
        extension: &str,
    ) -> Vec<(String, String, String)> {
        let mut all_packages = Vec::new();

        // Check if ext section exists and contains the extension
        if let Some(ext_section) = config.get("ext") {
            if let Some(ext_config) = ext_section.get(extension) {
                // Look for dependencies section
                if let Some(dependencies) = ext_config.get("dependencies") {
                    if let toml::Value::Table(deps_map) = dependencies {
                        for (package_name, package_spec) in deps_map {
                            let resolved_deps = self.resolve_package_dependencies(
                                config,
                                package_name,
                                package_spec,
                            );
                            all_packages.extend(resolved_deps);
                        }
                    }
                }
            }
        }

        // Remove duplicates while preserving order
        let mut seen = HashSet::new();
        let mut unique_packages = Vec::new();
        for (dep_type, pkg_name, pkg_version) in all_packages {
            let pkg_key = (dep_type.clone(), pkg_name.clone(), pkg_version.clone());
            if !seen.contains(&pkg_key) {
                seen.insert(pkg_key);
                unique_packages.push((dep_type, pkg_name, pkg_version));
            }
        }

        // Sort: extensions first, then packages, both alphabetically
        unique_packages.sort_by(|a, b| {
            let a_is_ext = a.0 == "ext";
            let b_is_ext = b.0 == "ext";
            match (a_is_ext, b_is_ext) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.1.cmp(&b.1),
            }
        });

        unique_packages
    }
}

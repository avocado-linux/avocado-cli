use anyhow::Result;
use std::collections::HashSet;

use crate::utils::config::{Config, ExtensionLocation};
use crate::utils::output::{print_error, print_info, OutputLevel};
use crate::utils::target::resolve_target_required;

pub struct ExtDepsCommand {
    config_path: String,
    extension: Option<String>,
    target: Option<String>,
}

impl ExtDepsCommand {
    pub fn new(config_path: String, extension: Option<String>, target: Option<String>) -> Self {
        Self {
            config_path,
            extension,
            target,
        }
    }

    pub fn execute(&self) -> Result<()> {
        let config = Config::load(&self.config_path)?;
        let content = std::fs::read_to_string(&self.config_path)?;
        let parsed: toml::Value = toml::from_str(&content)?;

        let target = resolve_target_required(self.target.as_deref(), &config)?;
        let extensions_to_process = self.get_extensions_to_process(&config, &parsed, &target)?;

        self.display_dependencies(&parsed, &extensions_to_process);
        Ok(())
    }

    fn get_extensions_to_process(
        &self,
        config: &Config,
        parsed: &toml::Value,
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
                    Some(location) => {
                        if let ExtensionLocation::External { name, config_path } = &location {
                            print_info(
                                &format!(
                                    "Found external extension '{name}' in config '{config_path}'"
                                ),
                                OutputLevel::Normal,
                            );
                        }
                        Ok(vec![extension_name.clone()])
                    }
                    None => {
                        self.print_extension_not_found(extension_name);
                        Err(anyhow::anyhow!("Extension not found"))
                    }
                }
            }
            None => {
                // For listing all extensions, still use local extensions only
                let ext_section = parsed.get("ext");
                match ext_section {
                    Some(ext) => {
                        let ext_table = ext
                            .as_table()
                            .ok_or_else(|| anyhow::anyhow!("Invalid ext section format"))?;
                        Ok(ext_table.keys().cloned().collect())
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

    fn display_dependencies(&self, parsed: &toml::Value, extensions: &[String]) {
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
        config: &toml::Value,
        package_name: &str,
        package_spec: &toml::Value,
    ) -> Vec<(String, String, String)> {
        match package_spec {
            toml::Value::String(version) => {
                vec![("pkg".to_string(), package_name.to_string(), version.clone())]
            }
            toml::Value::Table(spec_map) => {
                self.resolve_table_dependency(config, package_name, spec_map)
            }
            _ => Vec::new(),
        }
    }

    fn resolve_table_dependency(
        &self,
        config: &toml::Value,
        package_name: &str,
        spec_map: &toml::Table,
    ) -> Vec<(String, String, String)> {
        // Try version first
        if let Some(toml::Value::String(version)) = spec_map.get("version") {
            return vec![("pkg".to_string(), package_name.to_string(), version.clone())];
        }

        // Try extension reference
        if let Some(toml::Value::String(ext_name)) = spec_map.get("ext") {
            return self.resolve_extension_dependency(config, ext_name);
        }

        // Try compile reference
        if let Some(toml::Value::String(compile_name)) = spec_map.get("compile") {
            return self.resolve_compile_dependencies(config, compile_name);
        }

        Vec::new()
    }

    fn resolve_extension_dependency(
        &self,
        config: &toml::Value,
        ext_name: &str,
    ) -> Vec<(String, String, String)> {
        let version = config
            .get("ext")
            .and_then(|ext_section| ext_section.get(ext_name))
            .and_then(|ext_config| ext_config.get("version"))
            .and_then(|v| v.as_str())
            .unwrap_or("*");

        vec![("ext".to_string(), ext_name.to_string(), version.to_string())]
    }

    fn resolve_compile_dependencies(
        &self,
        config: &toml::Value,
        compile_name: &str,
    ) -> Vec<(String, String, String)> {
        let compile_deps = config
            .get("sdk")
            .and_then(|sdk| sdk.get("compile"))
            .and_then(|compile| compile.get(compile_name))
            .and_then(|compile_config| compile_config.get("dependencies"))
            .and_then(|deps| deps.as_table());

        let Some(deps_table) = compile_deps else {
            return Vec::new();
        };

        deps_table
            .iter()
            .filter_map(|(dep_name, dep_spec)| self.extract_dependency_version(dep_name, dep_spec))
            .collect()
    }

    fn extract_dependency_version(
        &self,
        dep_name: &str,
        dep_spec: &toml::Value,
    ) -> Option<(String, String, String)> {
        match dep_spec {
            toml::Value::String(version) => {
                Some(("pkg".to_string(), dep_name.to_string(), version.clone()))
            }
            toml::Value::Table(spec_map) => spec_map
                .get("version")
                .and_then(|v| v.as_str())
                .map(|version| ("pkg".to_string(), dep_name.to_string(), version.to_string())),
            _ => None,
        }
    }

    fn list_packages_from_config(
        &self,
        config: &toml::Value,
        extension: &str,
    ) -> Vec<(String, String, String)> {
        let dependencies = config
            .get("ext")
            .and_then(|ext_section| ext_section.get(extension))
            .and_then(|ext_config| ext_config.get("dependencies"))
            .and_then(|deps| deps.as_table());

        let Some(deps_table) = dependencies else {
            return Vec::new();
        };

        let mut all_packages: Vec<_> = deps_table
            .iter()
            .flat_map(|(package_name, package_spec)| {
                self.resolve_package_dependencies(config, package_name, package_spec)
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

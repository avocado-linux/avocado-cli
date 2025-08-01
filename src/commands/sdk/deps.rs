//! SDK deps command implementation.

use anyhow::{Context, Result};
use std::collections::HashSet;

use crate::utils::{config::Config, output::print_success};

/// Implementation of the 'sdk deps' command.
pub struct SdkDepsCommand {
    /// Path to configuration file
    pub config_path: String,
}

impl SdkDepsCommand {
    /// Create a new SdkDepsCommand instance
    pub fn new(config_path: String) -> Self {
        Self { config_path }
    }

    /// Execute the sdk deps command
    pub fn execute(&self) -> Result<()> {
        // Load the configuration
        let config = Config::load(&self.config_path)
            .with_context(|| format!("Failed to load config from {}", self.config_path))?;

        // List packages from config
        let packages = self.list_packages_from_config(&config);

        for (dep_type, pkg_name, pkg_version) in &packages {
            println!("({}) {} ({})", dep_type, pkg_name, pkg_version);
        }

        // Print success message with count
        let count = packages.len();
        print_success(&format!("Listed {} dependency(s).", count));

        Ok(())
    }

    /// List all packages from SDK dependencies and compile dependencies in config
    fn list_packages_from_config(&self, config: &Config) -> Vec<(String, String, String)> {
        let mut all_packages = Vec::new();

        // Process SDK dependencies
        if let Some(sdk_deps) = config.get_sdk_dependencies() {
            for (package_name, package_spec) in sdk_deps {
                let resolved_deps =
                    self.resolve_package_dependencies(config, package_name, package_spec);
                all_packages.extend(resolved_deps);
            }
        }

        // Process compile dependencies
        let compile_dependencies = config.get_compile_dependencies();
        for (_section_name, dependencies) in compile_dependencies {
            for (package_name, package_spec) in dependencies {
                let resolved_deps =
                    self.resolve_package_dependencies(config, package_name, package_spec);
                all_packages.extend(resolved_deps);
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
            match (a.0.as_str(), b.0.as_str()) {
                ("ext", "pkg") => std::cmp::Ordering::Less,
                ("pkg", "ext") => std::cmp::Ordering::Greater,
                _ => a.1.cmp(&b.1), // Sort by package name alphabetically
            }
        });

        unique_packages
    }

    /// Resolve dependencies for a package specification
    fn resolve_package_dependencies(
        &self,
        config: &Config,
        package_name: &str,
        package_spec: &toml::Value,
    ) -> Vec<(String, String, String)> {
        let mut dependencies = Vec::new();

        match package_spec {
            toml::Value::String(version) => {
                // Simple string version: "package-name = version"
                dependencies.push(("pkg".to_string(), package_name.to_string(), version.clone()));
            }
            toml::Value::Table(table) => {
                if let Some(toml::Value::String(version)) = table.get("version") {
                    // Object with version: "package-name = { version = "1.0.0" }"
                    dependencies.push((
                        "pkg".to_string(),
                        package_name.to_string(),
                        version.clone(),
                    ));
                } else if let Some(toml::Value::String(ext_name)) = table.get("ext") {
                    // Extension reference
                    let version = self.get_extension_version(config, ext_name);
                    dependencies.push(("ext".to_string(), ext_name.clone(), version));
                } else if let Some(toml::Value::String(compile_name)) = table.get("compile") {
                    // Object with compile reference - only install the compile dependencies, not the package itself
                    let compile_deps = self.get_compile_dependencies(config, compile_name);
                    dependencies.extend(compile_deps);
                }
            }
            _ => {
                // Default case: treat as package with wildcard version
                dependencies.push(("pkg".to_string(), package_name.to_string(), "*".to_string()));
            }
        }

        dependencies
    }

    /// Get version for an extension from config
    fn get_extension_version(&self, _config: &Config, _ext_name: &str) -> String {
        // TODO: Implement extension version lookup when extension support is added
        "*".to_string()
    }

    /// Get compile dependencies for a compile section
    fn get_compile_dependencies(
        &self,
        config: &Config,
        compile_name: &str,
    ) -> Vec<(String, String, String)> {
        let mut dependencies = Vec::new();

        if let Some(sdk) = &config.sdk {
            if let Some(compile) = &sdk.compile {
                if let Some(compile_config) = compile.get(compile_name) {
                    if let Some(deps) = &compile_config.dependencies {
                        for (dep_name, dep_spec) in deps {
                            match dep_spec {
                                toml::Value::String(version) => {
                                    dependencies.push((
                                        "pkg".to_string(),
                                        dep_name.clone(),
                                        version.clone(),
                                    ));
                                }
                                toml::Value::Table(table) => {
                                    if let Some(toml::Value::String(version)) = table.get("version")
                                    {
                                        dependencies.push((
                                            "pkg".to_string(),
                                            dep_name.clone(),
                                            version.clone(),
                                        ));
                                    }
                                }
                                _ => {
                                    dependencies.push((
                                        "pkg".to_string(),
                                        dep_name.clone(),
                                        "*".to_string(),
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }

        dependencies
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_new() {
        let cmd = SdkDepsCommand::new("config.toml".to_string());
        assert_eq!(cmd.config_path, "config.toml");
    }

    #[test]
    fn test_resolve_package_dependencies() {
        let cmd = SdkDepsCommand::new("test.toml".to_string());

        // Create a minimal config for testing
        let config_content = r#"
[sdk]
image = "test-image"

[sdk.dependencies]
cmake = "*"
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{}", config_content).unwrap();
        let config = Config::load(temp_file.path()).unwrap();

        // Test string version
        let deps = cmd.resolve_package_dependencies(
            &config,
            "test-package",
            &toml::Value::String("1.0.0".to_string()),
        );
        assert_eq!(deps.len(), 1);
        assert_eq!(
            deps[0],
            (
                "pkg".to_string(),
                "test-package".to_string(),
                "1.0.0".to_string()
            )
        );

        // Test table with version
        let mut table = toml::map::Map::new();
        table.insert(
            "version".to_string(),
            toml::Value::String("2.0.0".to_string()),
        );
        let deps =
            cmd.resolve_package_dependencies(&config, "test-package2", &toml::Value::Table(table));
        assert_eq!(deps.len(), 1);
        assert_eq!(
            deps[0],
            (
                "pkg".to_string(),
                "test-package2".to_string(),
                "2.0.0".to_string()
            )
        );
    }

    #[test]
    fn test_list_packages_from_config() {
        let cmd = SdkDepsCommand::new("test.toml".to_string());

        let config_content = r#"
[sdk]
image = "test-image"

[sdk.dependencies]
cmake = "*"
gcc = "11.0.0"

[sdk.compile.app]
dependencies = { make = "4.3" }
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{}", config_content).unwrap();
        let config = Config::load(temp_file.path()).unwrap();

        let packages = cmd.list_packages_from_config(&config);

        // Should have 3 packages: cmake, gcc, and make
        assert_eq!(packages.len(), 3);

        // Verify packages exist (order may vary due to sorting)
        let package_names: Vec<&String> = packages.iter().map(|(_, name, _)| name).collect();
        assert!(package_names.contains(&&"cmake".to_string()));
        assert!(package_names.contains(&&"gcc".to_string()));
        assert!(package_names.contains(&&"make".to_string()));
    }
}

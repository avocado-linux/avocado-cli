//! SDK deps command implementation.

use anyhow::{Context, Result};
use std::collections::HashSet;

use crate::utils::{
    config::Config,
    output::{print_success, OutputLevel},
};

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
        let config = Config::load(&self.config_path)
            .with_context(|| format!("Failed to load config from {}", self.config_path))?;

        let packages = self.list_packages_from_config(&config);
        self.display_packages(&packages);

        print_success(
            &format!("Listed {} dependency(s).", packages.len()),
            OutputLevel::Normal,
        );

        Ok(())
    }

    fn display_packages(&self, packages: &[(String, String, String)]) {
        for (dep_type, pkg_name, pkg_version) in packages {
            println!("({}) {} ({})", dep_type, pkg_name, pkg_version);
        }
    }

    /// List all packages from SDK dependencies and compile dependencies in config
    fn list_packages_from_config(&self, config: &Config) -> Vec<(String, String, String)> {
        let mut all_packages = Vec::new();

        // Process SDK dependencies
        self.collect_sdk_dependencies(config, &mut all_packages);

        // Process compile dependencies
        self.collect_compile_dependencies(config, &mut all_packages);

        self.deduplicate_and_sort(all_packages)
    }

    fn collect_sdk_dependencies(
        &self,
        config: &Config,
        packages: &mut Vec<(String, String, String)>,
    ) {
        if let Some(sdk_deps) = config.get_sdk_dependencies() {
            for (package_name, package_spec) in sdk_deps {
                let resolved_deps =
                    self.resolve_package_dependencies(config, package_name, package_spec);
                packages.extend(resolved_deps);
            }
        }
    }

    fn collect_compile_dependencies(
        &self,
        config: &Config,
        packages: &mut Vec<(String, String, String)>,
    ) {
        let compile_dependencies = config.get_compile_dependencies();
        for (_section_name, dependencies) in compile_dependencies {
            for (package_name, package_spec) in dependencies {
                let resolved_deps =
                    self.resolve_package_dependencies(config, package_name, package_spec);
                packages.extend(resolved_deps);
            }
        }
    }

    fn deduplicate_and_sort(
        &self,
        packages: Vec<(String, String, String)>,
    ) -> Vec<(String, String, String)> {
        let mut seen = HashSet::new();
        let mut unique_packages = Vec::new();

        for package in packages {
            if seen.insert(package.clone()) {
                unique_packages.push(package);
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
        match package_spec {
            toml::Value::String(version) => {
                vec![("pkg".to_string(), package_name.to_string(), version.clone())]
            }
            toml::Value::Table(table) => self.resolve_table_dependency(config, package_name, table),
            _ => {
                // Default case: treat as package with wildcard version
                vec![("pkg".to_string(), package_name.to_string(), "*".to_string())]
            }
        }
    }

    fn resolve_table_dependency(
        &self,
        config: &Config,
        package_name: &str,
        table: &toml::Table,
    ) -> Vec<(String, String, String)> {
        // Try version first
        if let Some(toml::Value::String(version)) = table.get("version") {
            return vec![("pkg".to_string(), package_name.to_string(), version.clone())];
        }

        // Try extension reference
        if let Some(toml::Value::String(ext_name)) = table.get("ext") {
            let version = self.get_extension_version(config, ext_name);
            return vec![("ext".to_string(), ext_name.clone(), version)];
        }

        // Try compile reference
        if let Some(toml::Value::String(compile_name)) = table.get("compile") {
            return self.get_compile_dependencies(config, compile_name);
        }

        // Default case
        vec![("pkg".to_string(), package_name.to_string(), "*".to_string())]
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
        let compile_deps = config
            .sdk
            .as_ref()
            .and_then(|sdk| sdk.compile.as_ref())
            .and_then(|compile| compile.get(compile_name))
            .and_then(|compile_config| compile_config.dependencies.as_ref());

        let Some(deps) = compile_deps else {
            return Vec::new();
        };

        deps.iter()
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
            toml::Value::Table(table) => table
                .get("version")
                .and_then(|v| v.as_str())
                .map(|version| ("pkg".to_string(), dep_name.to_string(), version.to_string())),
            _ => Some(("pkg".to_string(), dep_name.to_string(), "*".to_string())),
        }
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

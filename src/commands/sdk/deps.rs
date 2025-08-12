//! SDK deps command implementation.

use anyhow::{Context, Result};
use std::collections::HashMap;

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

        // Read the config file content for extension parsing
        let config_content = std::fs::read_to_string(&self.config_path)
            .with_context(|| format!("Failed to read config file {}", self.config_path))?;

        let sections = self.list_packages_by_section(&config, &config_content)?;
        let total_count = self.display_packages_by_section(&sections);

        print_success(
            &format!("Listed {} dependency(s).", total_count),
            OutputLevel::Normal,
        );

        Ok(())
    }

    fn display_packages_by_section(&self, sections: &HashMap<String, Vec<(String, String, String)>>) -> usize {
        let mut total_count = 0;
        let mut first_section = true;

        // Define section order for consistent output
        let section_order = vec![
            "SDK Dependencies".to_string(),
            "Compile Dependencies".to_string(),
        ];

        // Display ordered sections first
        for section_name in &section_order {
            if let Some(packages) = sections.get(section_name) {
                if !packages.is_empty() {
                    if !first_section {
                        println!();
                    }
                    first_section = false;

                    println!("\x1b[1;37m{}\x1b[0m", section_name);
                    for (dep_type, pkg_name, pkg_version) in packages {
                        println!("({dep_type}) {pkg_name} ({pkg_version})");
                        total_count += 1;
                    }
                }
            }
        }

        // Display extension sections (sorted alphabetically)
        let mut extension_sections: Vec<_> = sections
            .iter()
            .filter(|(name, _)| !section_order.contains(name))
            .collect();
        extension_sections.sort_by_key(|(name, _)| name.as_str());

        for (section_name, packages) in extension_sections {
            if !packages.is_empty() {
                if !first_section {
                    println!();
                }
                first_section = false;

                println!("\x1b[1;37m{}\x1b[0m", section_name);
                for (dep_type, pkg_name, pkg_version) in packages {
                    println!("({dep_type}) {pkg_name} ({pkg_version})");
                    total_count += 1;
                }
            }
        }

        total_count
    }

    /// List all packages grouped by section
    fn list_packages_by_section(&self, config: &Config, config_content: &str) -> Result<HashMap<String, Vec<(String, String, String)>>> {
        let mut sections = HashMap::new();

        // Process SDK dependencies
        self.collect_sdk_dependencies_by_section(config, &mut sections);

        // Process extension SDK dependencies
        self.collect_extension_sdk_dependencies_by_section(config, config_content, &mut sections)?;

        // Process compile dependencies
        self.collect_compile_dependencies_by_section(config, &mut sections);

        // Sort packages within each section
        for (_, packages) in sections.iter_mut() {
            packages.sort_by(|a, b| a.1.cmp(&b.1)); // Sort by package name
        }

        Ok(sections)
    }

    fn collect_sdk_dependencies_by_section(
        &self,
        config: &Config,
        sections: &mut HashMap<String, Vec<(String, String, String)>>,
    ) {
        if let Some(sdk_deps) = config.get_sdk_dependencies() {
            let section_packages = sections.entry("SDK Dependencies".to_string()).or_insert_with(Vec::new);
            for (package_name, package_spec) in sdk_deps {
                let resolved_deps =
                    self.resolve_package_dependencies(config, package_name, package_spec);
                section_packages.extend(resolved_deps);
            }
        }
    }

    fn collect_extension_sdk_dependencies_by_section(
        &self,
        config: &Config,
        config_content: &str,
        sections: &mut HashMap<String, Vec<(String, String, String)>>,
    ) -> Result<()> {
        let extension_sdk_deps = config.get_extension_sdk_dependencies(config_content)?;

        for (ext_name, dependencies) in extension_sdk_deps {
            let section_name = format!("Extension SDK Dependencies ({})", ext_name);
            let section_packages = sections.entry(section_name).or_insert_with(Vec::new);
            for (package_name, package_spec) in dependencies {
                let resolved_deps =
                    self.resolve_package_dependencies(config, &package_name, &package_spec);
                section_packages.extend(resolved_deps);
            }
        }
        Ok(())
    }

    fn collect_compile_dependencies_by_section(
        &self,
        config: &Config,
        sections: &mut HashMap<String, Vec<(String, String, String)>>,
    ) {
        let compile_dependencies = config.get_compile_dependencies();
        if !compile_dependencies.is_empty() {
            let section_packages = sections.entry("Compile Dependencies".to_string()).or_insert_with(Vec::new);
            for (_section_name, dependencies) in compile_dependencies {
                for (package_name, package_spec) in dependencies {
                    let resolved_deps =
                        self.resolve_package_dependencies(config, package_name, package_spec);
                    section_packages.extend(resolved_deps);
                }
            }
        }
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
        write!(temp_file, "{config_content}").unwrap();
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
    fn test_list_packages_by_section() {
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
        write!(temp_file, "{config_content}").unwrap();
        let config = Config::load(temp_file.path()).unwrap();

        let sections = cmd.list_packages_by_section(&config, config_content).unwrap();

        // Should have 2 sections: SDK Dependencies and Compile Dependencies
        assert_eq!(sections.len(), 2);

        // Check SDK Dependencies section
        let sdk_packages = sections.get("SDK Dependencies").unwrap();
        assert_eq!(sdk_packages.len(), 2);
        let sdk_package_names: Vec<&String> = sdk_packages.iter().map(|(_, name, _)| name).collect();
        assert!(sdk_package_names.contains(&&"cmake".to_string()));
        assert!(sdk_package_names.contains(&&"gcc".to_string()));

        // Check Compile Dependencies section
        let compile_packages = sections.get("Compile Dependencies").unwrap();
        assert_eq!(compile_packages.len(), 1);
        let compile_package_names: Vec<&String> = compile_packages.iter().map(|(_, name, _)| name).collect();
        assert!(compile_package_names.contains(&&"make".to_string()));
    }

    #[test]
    fn test_multiple_extensions_with_same_dependency() {
        let cmd = SdkDepsCommand::new("test.toml".to_string());

        let config_content = r#"
[sdk]
image = "test-image"

[sdk.dependencies]
cmake = "*"

[ext.avocado-dev]
sysext = true
confext = true

[ext.avocado-dev.sdk.dependencies]
nativesdk-avocado-hitl = "*"

[ext.avocado-dev1]
sysext = true
confext = true

[ext.avocado-dev1.sdk.dependencies]
nativesdk-avocado-hitl = "*"
"#;
        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{config_content}").unwrap();
        let config = Config::load(temp_file.path()).unwrap();

        let sections = cmd.list_packages_by_section(&config, config_content).unwrap();

        // Should have 3 sections: SDK Dependencies and 2 Extension sections
        assert_eq!(sections.len(), 3);

        // Check SDK Dependencies section
        let sdk_packages = sections.get("SDK Dependencies").unwrap();
        assert_eq!(sdk_packages.len(), 1);
        let sdk_package_names: Vec<&String> = sdk_packages.iter().map(|(_, name, _)| name).collect();
        assert!(sdk_package_names.contains(&&"cmake".to_string()));

        // Check first extension
        let ext1_packages = sections.get("Extension SDK Dependencies (avocado-dev)").unwrap();
        assert_eq!(ext1_packages.len(), 1);
        let ext1_package_names: Vec<&String> = ext1_packages.iter().map(|(_, name, _)| name).collect();
        assert!(ext1_package_names.contains(&&"nativesdk-avocado-hitl".to_string()));

        // Check second extension
        let ext2_packages = sections.get("Extension SDK Dependencies (avocado-dev1)").unwrap();
        assert_eq!(ext2_packages.len(), 1);
        let ext2_package_names: Vec<&String> = ext2_packages.iter().map(|(_, name, _)| name).collect();
        assert!(ext2_package_names.contains(&&"nativesdk-avocado-hitl".to_string()));
    }
}

use anyhow::Result;
use std::sync::Arc;

use crate::utils::config::{get_comp_role, ComponentRole, ComposedConfig, Config};
use crate::utils::output::{print_success, OutputLevel};

/// `avocado comp list` — print every component defined in the merged config
/// together with its role. Mirrors `avocado ext list`.
pub struct CompListCommand {
    config_path: String,
    /// Pre-composed configuration to avoid reloading.
    composed_config: Option<Arc<ComposedConfig>>,
}

impl CompListCommand {
    pub fn new(config_path: String) -> Self {
        Self {
            config_path,
            composed_config: None,
        }
    }

    #[allow(dead_code)]
    pub fn with_composed_config(mut self, config: Arc<ComposedConfig>) -> Self {
        self.composed_config = Some(config);
        self
    }

    pub fn execute(&self) -> Result<()> {
        let composed = match &self.composed_config {
            Some(cc) => Arc::clone(cc),
            None => Arc::new(Config::load_composed(&self.config_path, None)?),
        };
        let parsed = &composed.merged_value;

        let components = self.get_components(parsed);
        self.display_components(&components);

        print_success(
            &format!("Listed {} component(s).", components.len()),
            OutputLevel::Normal,
        );

        Ok(())
    }

    fn get_components(&self, parsed: &serde_yaml::Value) -> Vec<(String, Option<ComponentRole>)> {
        parsed
            .get("components")
            .and_then(|section| section.as_mapping())
            .map(|table| {
                table
                    .iter()
                    .filter_map(|(k, v)| {
                        let name = k.as_str()?.to_string();
                        Some((name, get_comp_role(v)))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn display_components(&self, components: &[(String, Option<ComponentRole>)]) {
        for (name, role) in components {
            match role {
                Some(role) => println!("{name}\t{}", format_role(*role)),
                None => println!("{name}\t(no role)"),
            }
        }
    }
}

fn format_role(role: ComponentRole) -> &'static str {
    match role {
        ComponentRole::Basefs => "basefs",
        ComponentRole::Initramfs => "initramfs",
        ComponentRole::Kernel => "kernel",
        ComponentRole::Other => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_list_returns_components_with_roles() {
        let yaml = r#"
sdk:
  image: test-image

components:
  avocado-comp-rootfs:
    role: basefs
  avocado-comp-initramfs:
    role: initramfs
  avocado-comp-kernel:
    role: kernel
  comp-no-role:
    image:
      type: kab
"#;
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{yaml}").unwrap();

        let cmd = CompListCommand::new(file.path().to_string_lossy().into_owned());
        let composed = Config::load_composed(file.path(), None).unwrap();
        let comps = cmd.get_components(&composed.merged_value);

        assert_eq!(comps.len(), 4);
        let roles: Vec<(String, Option<ComponentRole>)> = comps.into_iter().collect();
        assert!(roles.contains(&(
            "avocado-comp-rootfs".to_string(),
            Some(ComponentRole::Basefs)
        )));
        assert!(roles.contains(&(
            "avocado-comp-initramfs".to_string(),
            Some(ComponentRole::Initramfs)
        )));
        assert!(roles.contains(&(
            "avocado-comp-kernel".to_string(),
            Some(ComponentRole::Kernel)
        )));
        assert!(roles.contains(&("comp-no-role".to_string(), None)));
    }

    #[test]
    fn test_list_returns_empty_when_no_components_section() {
        let yaml = r#"
sdk:
  image: test-image
extensions:
  some-ext:
    version: "1.0.0"
"#;
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{yaml}").unwrap();

        let cmd = CompListCommand::new(file.path().to_string_lossy().into_owned());
        let composed = Config::load_composed(file.path(), None).unwrap();
        let comps = cmd.get_components(&composed.merged_value);

        assert!(comps.is_empty());
    }
}

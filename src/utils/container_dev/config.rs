//! Typed configuration for a runtime's `container_dev` block.
//!
//! The feature is gated structurally under the runtime: presence of a
//! `runtimes.<name>.container_dev` block enables Container Dev Mode for that
//! runtime; an absent block means the feature is off. A `container_dev` block
//! placed anywhere other than under a runtime is not honored — only the typed
//! [`RuntimeConfig::container_dev`] field enables the feature.

use serde::{Deserialize, Serialize};

/// Default registry port for Container Dev Mode.
///
/// Phase 0 task 1.6 chose a non-conflicting default: `5000` collides with the
/// macOS AirPlay Receiver, so it is explicitly avoided. Recorded in
/// `docs/container-dev/phase0-findings.md`.
pub const DEFAULT_REGISTRY_PORT: u16 = 5599;

fn default_registry_port() -> u16 {
    DEFAULT_REGISTRY_PORT
}

/// Container Dev Mode configuration for a runtime.
///
/// Parsed from `runtimes.<name>.container_dev`. Its mere presence enables the
/// feature for the owning runtime.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ContainerDevConfig {
    /// Images to watch on the host engine and hot-reload on the device.
    #[serde(default)]
    pub images: Vec<ContainerDevImage>,
    /// Embedded registry settings.
    #[serde(default)]
    pub registry: RegistryConfig,
}

/// A single watched image and the device service that consumes it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContainerDevImage {
    /// Image reference (`repository[:tag]`) watched on the host engine.
    #[serde(rename = "ref")]
    pub image_ref: String,
    /// Device service consuming the image.
    pub service: String,
}

/// Embedded registry settings for Container Dev Mode.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RegistryConfig {
    /// Port the bulk read listener binds. Defaults to
    /// [`DEFAULT_REGISTRY_PORT`] when omitted.
    #[serde(default = "default_registry_port")]
    pub port: u16,
}

impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_REGISTRY_PORT,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::config::{Config, RuntimeConfig};

    fn runtime_from(yaml: &str) -> RuntimeConfig {
        serde_yaml::from_str(yaml).expect("runtime config parses")
    }

    #[test]
    fn absent_block_leaves_feature_off() {
        let runtime = runtime_from("target: qemux86-64\n");
        assert!(
            runtime.container_dev.is_none(),
            "a runtime with no container_dev block must leave the feature off"
        );
    }

    #[test]
    fn present_block_enables_feature_and_parses_images() {
        let runtime = runtime_from(
            r#"
target: qemux86-64
container_dev:
  images:
    - ref: my-app:dev
      service: app
    - ref: sidecar:latest
      service: sidecar
  registry:
    port: 6001
"#,
        );

        let cd = runtime
            .container_dev
            .expect("a present container_dev block enables the feature");
        assert_eq!(cd.images.len(), 2);
        assert_eq!(cd.images[0].image_ref, "my-app:dev");
        assert_eq!(cd.images[0].service, "app");
        assert_eq!(cd.images[1].image_ref, "sidecar:latest");
        assert_eq!(cd.images[1].service, "sidecar");
        assert_eq!(cd.registry.port, 6001);
    }

    #[test]
    fn registry_port_defaults_to_phase0_literal_not_5000() {
        // registry block present but port omitted
        let runtime = runtime_from(
            r#"
container_dev:
  images: []
  registry: {}
"#,
        );
        let cd = runtime.container_dev.unwrap();
        assert_eq!(cd.registry.port, DEFAULT_REGISTRY_PORT);
        assert_ne!(cd.registry.port, 5000, "default port must not be 5000");

        // registry block entirely absent
        let runtime = runtime_from("container_dev:\n  images: []\n");
        let cd = runtime.container_dev.unwrap();
        assert_eq!(cd.registry.port, DEFAULT_REGISTRY_PORT);
        assert_ne!(cd.registry.port, 5000, "default port must not be 5000");
    }

    #[test]
    fn default_registry_port_is_not_5000() {
        assert_ne!(DEFAULT_REGISTRY_PORT, 5000);
    }

    #[test]
    fn top_level_block_does_not_enable_the_feature() {
        // A container_dev block placed at the top level (not under a runtime)
        // must NOT enable the feature for any runtime.
        let config_content = r#"
container_dev:
  images:
    - ref: my-app:dev
      service: app
runtimes:
  dev:
    target: qemux86-64
"#;
        let parsed: serde_yaml::Value = serde_yaml::from_str(config_content).unwrap();
        let runtimes = parsed
            .get("runtimes")
            .and_then(|r| r.as_mapping())
            .expect("runtimes present");
        for (_name, runtime_value) in runtimes {
            let runtime: RuntimeConfig =
                serde_yaml::from_value(runtime_value.clone()).expect("runtime parses");
            assert!(
                runtime.container_dev.is_none(),
                "a top-level container_dev block must not enable the feature for a runtime"
            );
        }
    }

    #[test]
    fn container_dev_is_registered_as_a_known_runtime_key() {
        // The ref-scanner must NOT recurse into container_dev.images looking
        // for dependency refs. We embed a spec_map shaped like an external
        // extension reference inside container_dev; if the scanner recursed
        // into it, that ref would be discovered.
        let config_content = r#"
runtimes:
  dev:
    target: qemux86-64
    container_dev:
      images:
        - ref: my-app:dev
          service: app
      packages:
        poison:
          extensions: leaked-ext
          config: leaked/path
"#;
        let parsed: serde_yaml::Value = serde_yaml::from_str(config_content).unwrap();
        let refs = Config::discover_external_config_refs(&parsed);
        assert!(
            !refs.iter().any(|(ext, _)| ext == "leaked-ext"),
            "container_dev must be a known-runtime-key so the ref scanner does not recurse into it"
        );
    }
}

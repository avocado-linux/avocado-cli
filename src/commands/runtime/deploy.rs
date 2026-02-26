use crate::utils::{
    config::{ComposedConfig, Config},
    container::{RunConfig, SdkContainer},
    output::{print_info, print_success, OutputLevel},
    stamps::{generate_batch_read_stamps_script, validate_stamps_batch, StampRequirement},
    target::resolve_target_required,
    update_repo::{self, HashCollectionOutput},
};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;

const DEFAULT_DEPLOY_REPO_PORT: u16 = 8585;
const DEPLOY_STAGING_DIR: &str = ".avocado/deploy-staging";

/// Parsed representation of a device connection string.
///
/// Accepts formats: `host`, `user@host`, `host:port`, `user@host:port`
#[derive(Debug, Clone, PartialEq)]
struct DeviceSpec {
    user: String,
    host: String,
    port: Option<u16>,
}

impl DeviceSpec {
    fn parse(device: &str) -> Result<Self> {
        let (user, host_port) = if let Some(at_pos) = device.find('@') {
            let user = &device[..at_pos];
            anyhow::ensure!(!user.is_empty(), "Empty user in device string '{device}'");
            (user.to_string(), &device[at_pos + 1..])
        } else {
            ("root".to_string(), device)
        };

        anyhow::ensure!(
            !host_port.is_empty(),
            "Empty host in device string '{device}'"
        );

        let (host, port) = if let Some(colon_pos) = host_port.rfind(':') {
            let maybe_port = &host_port[colon_pos + 1..];
            match maybe_port.parse::<u16>() {
                Ok(p) => {
                    let h = &host_port[..colon_pos];
                    anyhow::ensure!(!h.is_empty(), "Empty host in device string '{device}'");
                    (h.to_string(), Some(p))
                }
                // Not a valid port number -- treat the whole thing as a hostname
                // (e.g. IPv6 addresses like ::1)
                Err(_) => (host_port.to_string(), None),
            }
        } else {
            (host_port.to_string(), None)
        };

        Ok(Self { user, host, port })
    }

    /// SSH destination in `user@host` form.
    fn ssh_destination(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }

    /// SSH port arguments: `["-p", "<port>"]` if a port was specified, empty otherwise.
    fn ssh_port_args(&self) -> String {
        match self.port {
            Some(p) => format!("-p {p}"),
            None => String::new(),
        }
    }
}

pub struct RuntimeDeployCommand {
    runtime_name: String,
    config_path: String,
    verbose: bool,
    target: Option<String>,
    device: String,
    container_args: Option<Vec<String>>,
    dnf_args: Option<Vec<String>>,
    no_stamps: bool,
    sdk_arch: Option<String>,
    /// Pre-composed configuration to avoid reloading
    composed_config: Option<Arc<ComposedConfig>>,
}

impl RuntimeDeployCommand {
    pub fn new(
        runtime_name: String,
        config_path: String,
        verbose: bool,
        target: Option<String>,
        device: String,
        container_args: Option<Vec<String>>,
        dnf_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            runtime_name,
            config_path,
            verbose,
            target,
            device,
            container_args,
            dnf_args,
            no_stamps: false,
            sdk_arch: None,
            composed_config: None,
        }
    }

    /// Set the no_stamps flag
    pub fn with_no_stamps(mut self, no_stamps: bool) -> Self {
        self.no_stamps = no_stamps;
        self
    }

    /// Set SDK container architecture for cross-arch emulation
    pub fn with_sdk_arch(mut self, sdk_arch: Option<String>) -> Self {
        self.sdk_arch = sdk_arch;
        self
    }

    /// Set pre-composed configuration to avoid reloading
    #[allow(dead_code)]
    pub fn with_composed_config(mut self, config: Arc<ComposedConfig>) -> Self {
        self.composed_config = Some(config);
        self
    }

    pub async fn execute(&self) -> Result<()> {
        let composed = match &self.composed_config {
            Some(cc) => Arc::clone(cc),
            None => Arc::new(Config::load_composed(
                &self.config_path,
                self.target.as_deref(),
            )?),
        };
        let config = &composed.config;
        let parsed = &composed.merged_value;

        let container_image = config
            .get_sdk_image()
            .context("No SDK container image specified in configuration")?;

        let runtime_config = parsed
            .get("runtimes")
            .context("No runtime configuration found")?;

        let runtime_spec = runtime_config.get(&self.runtime_name).with_context(|| {
            format!("Runtime '{}' not found in configuration", self.runtime_name)
        })?;

        let _config_target = runtime_spec
            .get("target")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let target_arch = resolve_target_required(self.target.as_deref(), config)?;

        let container_helper =
            SdkContainer::from_config(&self.config_path, config)?.verbose(self.verbose);

        // Validate stamps before proceeding (unless --no-stamps)
        if !self.no_stamps {
            let required = vec![StampRequirement::new(
                crate::utils::stamps::StampCommand::Provision,
                crate::utils::stamps::StampComponent::Runtime,
                Some(&self.runtime_name),
            )];

            let batch_script = generate_batch_read_stamps_script(&required);
            let run_config = RunConfig {
                container_image: container_image.to_string(),
                target: target_arch.clone(),
                command: batch_script,
                verbose: false,
                source_environment: true,
                interactive: false,
                sdk_arch: self.sdk_arch.clone(),
                ..Default::default()
            };

            let output = container_helper
                .run_in_container_with_output(run_config)
                .await?;

            let validation =
                validate_stamps_batch(&required, output.as_deref().unwrap_or(""), None);

            if !validation.is_satisfied() {
                validation
                    .into_error(&format!("Cannot deploy runtime '{}'", self.runtime_name))
                    .print_and_exit();
            }
        }

        print_info(
            &format!(
                "Deploying runtime '{}' to device '{}'",
                self.runtime_name, self.device
            ),
            OutputLevel::Normal,
        );

        // --- Phase 1: Hash collection ---
        print_info(
            "Phase 1: Collecting artifact hashes...",
            OutputLevel::Normal,
        );

        let hash_script = self.create_hash_collection_script(&target_arch);
        let hash_run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target_arch.clone(),
            command: hash_script,
            verbose: false,
            source_environment: true,
            interactive: false,
            container_args: config.merge_sdk_container_args(self.container_args.as_ref()),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };

        let hash_output = container_helper
            .run_in_container_with_output(hash_run_config)
            .await?
            .context("Hash collection script produced no output")?;

        let collection: HashCollectionOutput =
            serde_json::from_str(&hash_output).context("Failed to parse hash collection output")?;

        if self.verbose {
            print_info(
                &format!(
                    "Collected hashes for {} target file(s).",
                    collection.targets.len()
                ),
                OutputLevel::Normal,
            );
        }

        // --- Phase 2: Generate and sign TUF metadata ---
        print_info(
            "Phase 2: Generating signed TUF metadata...",
            OutputLevel::Normal,
        );

        let signing_key_name = config.get_runtime_signing_key_name(&self.runtime_name);
        let project_dir = std::path::Path::new(&self.config_path)
            .parent()
            .unwrap_or(std::path::Path::new("."));

        let (sk, pk) = crate::utils::update_signing::resolve_signing_key(
            signing_key_name.as_deref(),
            project_dir,
        )?;

        let repo_metadata = update_repo::generate_repo_metadata(&collection.targets, &sk, &pk)?;

        let staging_dir = project_dir.join(DEPLOY_STAGING_DIR);
        std::fs::create_dir_all(&staging_dir).with_context(|| {
            format!(
                "Failed to create deploy staging directory: {}",
                staging_dir.display()
            )
        })?;

        std::fs::write(
            staging_dir.join("targets.json"),
            &repo_metadata.targets_json,
        )
        .context("Failed to write targets.json")?;
        std::fs::write(
            staging_dir.join("snapshot.json"),
            &repo_metadata.snapshot_json,
        )
        .context("Failed to write snapshot.json")?;
        std::fs::write(
            staging_dir.join("timestamp.json"),
            &repo_metadata.timestamp_json,
        )
        .context("Failed to write timestamp.json")?;
        std::fs::write(staging_dir.join("root.json"), &collection.root_json)
            .context("Failed to write root.json to staging")?;

        if self.verbose {
            print_info(
                &format!("Wrote signed metadata to {}", staging_dir.display()),
                OutputLevel::Normal,
            );
        }

        // --- Phase 3: Serve repo and trigger update ---
        print_info(
            "Phase 3: Serving update repository and triggering device update...",
            OutputLevel::Normal,
        );

        let deploy_script = self.create_deploy_script(&target_arch)?;

        let mut env_vars = HashMap::new();
        env_vars.insert("AVOCADO_TARGET".to_string(), target_arch.clone());
        env_vars.insert("AVOCADO_SDK_TARGET".to_string(), target_arch.clone());
        env_vars.insert("AVOCADO_RUNTIME".to_string(), self.runtime_name.clone());
        env_vars.insert("AVOCADO_DEPLOY_MACHINE".to_string(), self.device.clone());

        if let Ok(repo_host) = std::env::var("AVOCADO_DEPLOY_REPO_HOST") {
            env_vars.insert("AVOCADO_DEPLOY_REPO_HOST".to_string(), repo_host);
        }
        if let Ok(repo_port) = std::env::var("AVOCADO_DEPLOY_REPO_PORT") {
            env_vars.insert("AVOCADO_DEPLOY_REPO_PORT".to_string(), repo_port);
        }

        let run_config = RunConfig {
            container_image: container_image.to_string(),
            target: target_arch.clone(),
            command: deploy_script,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            env_vars: Some(env_vars),
            container_args: config.merge_sdk_container_args(self.container_args.as_ref()),
            dnf_args: self.dnf_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };
        let deploy_result = container_helper
            .run_in_container(run_config)
            .await
            .context("Failed to deploy runtime")?;

        // Clean up staging directory
        let _ = std::fs::remove_dir_all(&staging_dir);

        if !deploy_result {
            return Err(anyhow::anyhow!("Failed to deploy runtime"));
        }

        print_success(
            &format!(
                "Successfully deployed runtime '{}' to device '{}'",
                self.runtime_name, self.device
            ),
            OutputLevel::Normal,
        );
        Ok(())
    }

    /// Phase 1: Generate a shell script that enumerates built artifacts,
    /// computes their SHA-256 hashes and sizes, and outputs structured JSON.
    fn create_hash_collection_script(&self, _target_arch: &str) -> String {
        format!(
            r#"
set -e

RUNTIME_NAME="{runtime_name}"
VAR_STAGING="$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME/var-staging"
IMAGES_DIR="$VAR_STAGING/lib/avocado/images"

# Find the active manifest (prefer active symlink over find)
ACTIVE_LINK="$VAR_STAGING/lib/avocado/active"
if [ -L "$ACTIVE_LINK" ]; then
    MANIFEST_FILE="$VAR_STAGING/lib/avocado/$(readlink "$ACTIVE_LINK")/manifest.json"
fi
if [ -z "$MANIFEST_FILE" ] || [ ! -f "$MANIFEST_FILE" ]; then
    MANIFEST_FILE=$(find "$VAR_STAGING/lib/avocado/runtimes" -name manifest.json -type f 2>/dev/null | head -n 1)
fi
if [ -z "$MANIFEST_FILE" ] || [ ! -f "$MANIFEST_FILE" ]; then
    echo "ERROR: No manifest.json found in $VAR_STAGING/lib/avocado/runtimes/" >&2
    exit 1
fi

# Read root.json
ROOT_JSON_FILE="$VAR_STAGING/lib/avocado/metadata/root.json"
if [ ! -f "$ROOT_JSON_FILE" ]; then
    echo "ERROR: No root.json found at $ROOT_JSON_FILE" >&2
    exit 1
fi

# Start building JSON output
echo -n '{{"targets":['

FIRST=true

# Hash the manifest
HASH=$(sha256sum "$MANIFEST_FILE" | awk '{{print $1}}')
SIZE=$(stat -c '%s' "$MANIFEST_FILE")
echo -n '{{"name":"manifest.json","sha256":"'"$HASH"'","size":'"$SIZE"'}}'
FIRST=false

# Hash all image .raw files (content-addressable by UUIDv5)
if [ -d "$IMAGES_DIR" ]; then
    for RAW_FILE in "$IMAGES_DIR"/*.raw; do
        [ -f "$RAW_FILE" ] || continue
        BASENAME=$(basename "$RAW_FILE")
        HASH=$(sha256sum "$RAW_FILE" | awk '{{print $1}}')
        SIZE=$(stat -c '%s' "$RAW_FILE")
        if [ "$FIRST" = "false" ]; then
            echo -n ','
        fi
        echo -n '{{"name":"'"$BASENAME"'","sha256":"'"$HASH"'","size":'"$SIZE"'}}'
        FIRST=false
    done
fi

# Read and escape root.json for embedding
ROOT_JSON_ESCAPED=$(python3 -c "import json,sys; print(json.dumps(open(sys.argv[1]).read()))" "$ROOT_JSON_FILE")

echo -n '],"root_json":'
echo -n "$ROOT_JSON_ESCAPED"
echo -n '}}'
"#,
            runtime_name = self.runtime_name,
        )
    }

    /// Phase 3: Generate a shell script that assembles the TUF repo,
    /// starts an HTTP server, SSHes into the device, and triggers the update.
    fn create_deploy_script(&self, _target_arch: &str) -> Result<String> {
        let port = DEFAULT_DEPLOY_REPO_PORT;
        let spec = DeviceSpec::parse(&self.device)?;

        let script = format!(
            r#"
set -e

RUNTIME_NAME="{runtime_name}"
SSH_DEST="{ssh_dest}"
SSH_PORT_ARGS="{ssh_port_args}"
DEVICE_HOST="{device_host}"
PORT="${{AVOCADO_DEPLOY_REPO_PORT:-{port}}}"
REPO_DIR="/tmp/avocado-deploy-repo"
VAR_STAGING="$AVOCADO_PREFIX/runtimes/$RUNTIME_NAME/var-staging"
IMAGES_DIR="$VAR_STAGING/lib/avocado/images"
STAGING_DIR="/opt/src/{staging_dir}"

# Clean up any previous repo
rm -rf "$REPO_DIR"
mkdir -p "$REPO_DIR/metadata" "$REPO_DIR/targets"

# Copy metadata from deploy-staging (signed in Phase 2)
cp "$STAGING_DIR/targets.json" "$REPO_DIR/metadata/targets.json"
cp "$STAGING_DIR/snapshot.json" "$REPO_DIR/metadata/snapshot.json"
cp "$STAGING_DIR/timestamp.json" "$REPO_DIR/metadata/timestamp.json"

# Copy root.json from build output
cp "$VAR_STAGING/lib/avocado/metadata/root.json" "$REPO_DIR/metadata/root.json"
cp "$VAR_STAGING/lib/avocado/metadata/1.root.json" "$REPO_DIR/metadata/1.root.json"

# Link manifest.json into targets/ (prefer active symlink over find)
ACTIVE_LINK="$VAR_STAGING/lib/avocado/active"
if [ -L "$ACTIVE_LINK" ]; then
    MANIFEST_FILE="$VAR_STAGING/lib/avocado/$(readlink "$ACTIVE_LINK")/manifest.json"
fi
if [ -z "$MANIFEST_FILE" ] || [ ! -f "$MANIFEST_FILE" ]; then
    MANIFEST_FILE=$(find "$VAR_STAGING/lib/avocado/runtimes" -name manifest.json -type f 2>/dev/null | head -n 1)
fi
if [ -n "$MANIFEST_FILE" ] && [ -f "$MANIFEST_FILE" ]; then
    cp "$MANIFEST_FILE" "$REPO_DIR/targets/manifest.json"
fi

# Link image .raw files (content-addressable by UUIDv5) into targets/
if [ -d "$IMAGES_DIR" ]; then
    for RAW_FILE in "$IMAGES_DIR"/*.raw; do
        [ -f "$RAW_FILE" ] || continue
        BASENAME=$(basename "$RAW_FILE")
        ln -sf "$RAW_FILE" "$REPO_DIR/targets/$BASENAME"
    done
fi

echo "TUF repository assembled at $REPO_DIR"
ls -la "$REPO_DIR/metadata/"
ls -la "$REPO_DIR/targets/"

# Start HTTP server bound to all interfaces so the device can reach it
python3 -m http.server "$PORT" --bind 0.0.0.0 --directory "$REPO_DIR" &
HTTP_PID=$!

cleanup() {{
    kill "$HTTP_PID" 2>/dev/null || true
    wait "$HTTP_PID" 2>/dev/null || true
    rm -rf "$REPO_DIR"
}}
trap cleanup EXIT

sleep 1

# Determine the IP the device should use to reach this HTTP server.
# AVOCADO_DEPLOY_REPO_HOST overrides all auto-detection (useful for QEMU
# user-mode networking where the host is at 10.0.2.2).
if [ -n "${{AVOCADO_DEPLOY_REPO_HOST:-}}" ]; then
    HOST_IP="$AVOCADO_DEPLOY_REPO_HOST"
else
    # When the SSH target is a loopback address the device is likely a QEMU
    # VM with port-forwarded SSH.  Inside the VM, 127.0.0.1 is the VM's own
    # loopback -- not the host -- so we must resolve a real non-loopback IP.
    case "$DEVICE_HOST" in
        127.*|localhost|::1)
            HOST_IP=$(ip -4 addr show scope global 2>/dev/null | awk '/inet /{{print $2}}' | cut -d/ -f1 | head -n 1)
            ;;
        *)
            HOST_IP=$(ip route get "$DEVICE_HOST" 2>/dev/null | awk '{{for(i=1;i<=NF;i++) if($i=="src") print $(i+1)}}' | head -n 1)
            if [ -z "$HOST_IP" ]; then
                HOST_IP=$(ip -4 addr show scope global 2>/dev/null | awk '/inet /{{print $2}}' | cut -d/ -f1 | head -n 1)
            fi
            ;;
    esac
fi

if [ -z "$HOST_IP" ]; then
    echo "ERROR: Could not determine host IP address" >&2
    echo "       Set AVOCADO_DEPLOY_REPO_HOST to the IP the device can reach this host on." >&2
    exit 1
fi

REPO_URL="http://${{HOST_IP}}:${{PORT}}"
echo "Serving TUF repository at $REPO_URL"
echo "Connecting to $SSH_DEST via SSH..."

# SSH into device and trigger update (disable set -e to capture exit code)
set +e
ssh -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null \
    -o ConnectTimeout=10 \
    -o LogLevel=ERROR \
    $SSH_PORT_ARGS \
    "$SSH_DEST" \
    "avocadoctl runtime add --url $REPO_URL"

UPDATE_EXIT=$?
set -e

if [ $UPDATE_EXIT -eq 0 ]; then
    echo "Device update completed successfully."
else
    echo "ERROR: Device update failed with exit code $UPDATE_EXIT" >&2
    exit $UPDATE_EXIT
fi
"#,
            runtime_name = self.runtime_name,
            ssh_dest = spec.ssh_destination(),
            ssh_port_args = spec.ssh_port_args(),
            device_host = spec.host,
            port = port,
            staging_dir = DEPLOY_STAGING_DIR,
        );

        Ok(script)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- DeviceSpec parsing tests ---

    #[test]
    fn test_device_spec_bare_host() {
        let spec = DeviceSpec::parse("192.168.1.100").unwrap();
        assert_eq!(spec.user, "root");
        assert_eq!(spec.host, "192.168.1.100");
        assert_eq!(spec.port, None);
        assert_eq!(spec.ssh_destination(), "root@192.168.1.100");
        assert_eq!(spec.ssh_port_args(), "");
    }

    #[test]
    fn test_device_spec_user_at_host() {
        let spec = DeviceSpec::parse("admin@10.0.0.1").unwrap();
        assert_eq!(spec.user, "admin");
        assert_eq!(spec.host, "10.0.0.1");
        assert_eq!(spec.port, None);
        assert_eq!(spec.ssh_destination(), "admin@10.0.0.1");
    }

    #[test]
    fn test_device_spec_host_with_port() {
        let spec = DeviceSpec::parse("127.0.0.1:2222").unwrap();
        assert_eq!(spec.user, "root");
        assert_eq!(spec.host, "127.0.0.1");
        assert_eq!(spec.port, Some(2222));
        assert_eq!(spec.ssh_destination(), "root@127.0.0.1");
        assert_eq!(spec.ssh_port_args(), "-p 2222");
    }

    #[test]
    fn test_device_spec_user_host_port() {
        let spec = DeviceSpec::parse("root@127.0.0.1:2222").unwrap();
        assert_eq!(spec.user, "root");
        assert_eq!(spec.host, "127.0.0.1");
        assert_eq!(spec.port, Some(2222));
        assert_eq!(spec.ssh_destination(), "root@127.0.0.1");
        assert_eq!(spec.ssh_port_args(), "-p 2222");
    }

    #[test]
    fn test_device_spec_hostname_no_port() {
        let spec = DeviceSpec::parse("device.local").unwrap();
        assert_eq!(spec.user, "root");
        assert_eq!(spec.host, "device.local");
        assert_eq!(spec.port, None);
    }

    #[test]
    fn test_device_spec_hostname_with_port() {
        let spec = DeviceSpec::parse("device.local:22").unwrap();
        assert_eq!(spec.user, "root");
        assert_eq!(spec.host, "device.local");
        assert_eq!(spec.port, Some(22));
    }

    #[test]
    fn test_device_spec_fqdn_user_port() {
        let spec = DeviceSpec::parse("deploy@edge.company.com:2200").unwrap();
        assert_eq!(spec.user, "deploy");
        assert_eq!(spec.host, "edge.company.com");
        assert_eq!(spec.port, Some(2200));
    }

    #[test]
    fn test_device_spec_empty_fails() {
        assert!(DeviceSpec::parse("").is_err());
    }

    #[test]
    fn test_device_spec_empty_user_fails() {
        assert!(DeviceSpec::parse("@host").is_err());
    }

    // --- RuntimeDeployCommand tests ---

    #[test]
    fn test_new() {
        let cmd = RuntimeDeployCommand::new(
            "test-runtime".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("x86_64".to_string()),
            "192.168.1.100".to_string(),
            None,
            None,
        );

        assert_eq!(cmd.runtime_name, "test-runtime");
        assert_eq!(cmd.config_path, "avocado.yaml");
        assert!(!cmd.verbose);
        assert_eq!(cmd.target, Some("x86_64".to_string()));
        assert_eq!(cmd.device, "192.168.1.100");
    }

    #[test]
    fn test_create_deploy_script_bare_host() {
        let cmd = RuntimeDeployCommand::new(
            "test-runtime".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("x86_64".to_string()),
            "device.local".to_string(),
            None,
            None,
        );

        let script = cmd.create_deploy_script("x86_64").unwrap();

        assert!(script.contains("RUNTIME_NAME=\"test-runtime\""));
        assert!(script.contains("SSH_DEST=\"root@device.local\""));
        assert!(script.contains("DEVICE_HOST=\"device.local\""));
        assert!(script.contains("SSH_PORT_ARGS=\"\""));
        assert!(script.contains("python3 -m http.server"));
        assert!(script.contains("avocadoctl runtime add --url"));
        assert!(script.contains("ssh -o StrictHostKeyChecking=no"));
    }

    #[test]
    fn test_create_deploy_script_with_user_and_port() {
        let cmd = RuntimeDeployCommand::new(
            "test-runtime".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("qemux86-64".to_string()),
            "root@127.0.0.1:2222".to_string(),
            None,
            None,
        );

        let script = cmd.create_deploy_script("qemux86-64").unwrap();

        assert!(script.contains("SSH_DEST=\"root@127.0.0.1\""));
        assert!(script.contains("SSH_PORT_ARGS=\"-p 2222\""));
        assert!(script.contains("DEVICE_HOST=\"127.0.0.1\""));
        assert!(script.contains("$SSH_PORT_ARGS"));
        assert!(script.contains("\"$SSH_DEST\""));

        // Loopback device host should use ip addr instead of ip route get
        assert!(script.contains("127.*|localhost"));
        assert!(script.contains("ip -4 addr show scope global"));
    }

    #[test]
    fn test_create_deploy_script_host_with_port_no_user() {
        let cmd = RuntimeDeployCommand::new(
            "rt".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("x86_64".to_string()),
            "10.0.0.42:2222".to_string(),
            None,
            None,
        );

        let script = cmd.create_deploy_script("x86_64").unwrap();

        assert!(script.contains("SSH_DEST=\"root@10.0.0.42\""));
        assert!(script.contains("SSH_PORT_ARGS=\"-p 2222\""));
        assert!(script.contains("DEVICE_HOST=\"10.0.0.42\""));
    }

    #[test]
    fn test_new_with_container_args() {
        let container_args = Some(vec![
            "--privileged".to_string(),
            "--network=host".to_string(),
        ]);
        let dnf_args = Some(vec!["--nogpgcheck".to_string()]);

        let cmd = RuntimeDeployCommand::new(
            "test-runtime".to_string(),
            "avocado.yaml".to_string(),
            true,
            Some("aarch64".to_string()),
            "192.168.1.50".to_string(),
            container_args.clone(),
            dnf_args.clone(),
        );

        assert_eq!(cmd.runtime_name, "test-runtime");
        assert!(cmd.verbose);
        assert_eq!(cmd.device, "192.168.1.50");
        assert_eq!(cmd.container_args, container_args);
        assert_eq!(cmd.dnf_args, dnf_args);
    }

    #[test]
    fn test_create_deploy_script_with_ip() {
        let cmd = RuntimeDeployCommand::new(
            "edge-runtime".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("qemux86-64".to_string()),
            "10.0.0.42".to_string(),
            None,
            None,
        );

        let script = cmd.create_deploy_script("qemux86-64").unwrap();

        assert!(script.contains("SSH_DEST=\"root@10.0.0.42\""));
        assert!(script.contains("DEVICE_HOST=\"10.0.0.42\""));
    }

    #[test]
    fn test_create_deploy_script_with_hostname() {
        let cmd = RuntimeDeployCommand::new(
            "production".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("aarch64".to_string()),
            "edge-device.company.com".to_string(),
            None,
            None,
        );

        let script = cmd.create_deploy_script("aarch64").unwrap();

        assert!(script.contains("SSH_DEST=\"root@edge-device.company.com\""));
        assert!(script.contains("DEVICE_HOST=\"edge-device.company.com\""));
    }

    #[test]
    fn test_hash_collection_script() {
        let cmd = RuntimeDeployCommand::new(
            "my-runtime".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("x86_64".to_string()),
            "192.168.1.10".to_string(),
            None,
            None,
        );

        let script = cmd.create_hash_collection_script("x86_64");

        assert!(script.contains("RUNTIME_NAME=\"my-runtime\""));
        assert!(script.contains("sha256sum"));
        assert!(script.contains("manifest.json"));
        assert!(script.contains("root.json"));
        assert!(script.contains(r#""targets":["#));
    }

    #[test]
    fn test_environment_variables_setup() {
        let cmd = RuntimeDeployCommand::new(
            "my-runtime".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("x86_64".to_string()),
            "192.168.1.10".to_string(),
            None,
            None,
        );

        let target_arch = "x86_64";
        let mut env_vars = HashMap::new();
        env_vars.insert("AVOCADO_TARGET".to_string(), target_arch.to_string());
        env_vars.insert("AVOCADO_SDK_TARGET".to_string(), target_arch.to_string());
        env_vars.insert("AVOCADO_RUNTIME".to_string(), cmd.runtime_name.clone());
        env_vars.insert("AVOCADO_DEPLOY_MACHINE".to_string(), cmd.device.clone());

        assert_eq!(env_vars.get("AVOCADO_TARGET"), Some(&"x86_64".to_string()));
        assert_eq!(
            env_vars.get("AVOCADO_SDK_TARGET"),
            Some(&"x86_64".to_string())
        );
        assert_eq!(
            env_vars.get("AVOCADO_RUNTIME"),
            Some(&"my-runtime".to_string())
        );
        assert_eq!(
            env_vars.get("AVOCADO_DEPLOY_MACHINE"),
            Some(&"192.168.1.10".to_string())
        );
        assert_eq!(env_vars.len(), 4);
    }

    #[test]
    fn test_deploy_script_supports_repo_host_override() {
        let cmd = RuntimeDeployCommand::new(
            "rt".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("x86_64".to_string()),
            "root@127.0.0.1:2222".to_string(),
            None,
            None,
        );

        let script = cmd.create_deploy_script("x86_64").unwrap();

        assert!(script.contains("AVOCADO_DEPLOY_REPO_HOST"));
        assert!(script.contains("HOST_IP=\"$AVOCADO_DEPLOY_REPO_HOST\""));
    }

    #[test]
    fn test_deploy_script_non_loopback_uses_ip_route() {
        let cmd = RuntimeDeployCommand::new(
            "rt".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("x86_64".to_string()),
            "192.168.1.50".to_string(),
            None,
            None,
        );

        let script = cmd.create_deploy_script("x86_64").unwrap();

        assert!(script.contains("ip route get \"$DEVICE_HOST\""));
    }

    #[test]
    fn test_deploy_script_binds_all_interfaces() {
        let cmd = RuntimeDeployCommand::new(
            "rt".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("x86_64".to_string()),
            "device".to_string(),
            None,
            None,
        );

        let script = cmd.create_deploy_script("x86_64").unwrap();

        assert!(script.contains("--bind 0.0.0.0"));
    }

    #[test]
    fn test_deploy_script_contains_cleanup() {
        let cmd = RuntimeDeployCommand::new(
            "test".to_string(),
            "avocado.yaml".to_string(),
            false,
            Some("x86_64".to_string()),
            "device".to_string(),
            None,
            None,
        );

        let script = cmd.create_deploy_script("x86_64").unwrap();

        assert!(script.contains("trap cleanup EXIT"));
        assert!(script.contains("kill \"$HTTP_PID\""));
    }
}

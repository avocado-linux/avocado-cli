//! Provisionable runtime bundles.
//!
//! `avocado provision --pack <out>` runs provisioning up to, not including,
//! the SDK's `avocado-provision-<target>` hook and saves what that hook
//! reads instead. `avocado provision --bundle <file|url>` replays it on
//! another machine (an on-site VM hub) with only the SDK installed: no
//! build tree, no extension sysroots.
//!
//! Layout (an uncompressed tar; the payload inside is already gzipped):
//!
//! ```text
//! bundle.json       format, target, runtime, the hook's env snapshot, payload sha256
//! avocado.yaml      so `avocado sdk install` gets the matching SDK
//! avocado.lock
//! payload.tar.gz    paths under $AVOCADO_PREFIX the hook reads:
//!                   output/runtimes/<rt>/stone (sans _build), the var image,
//!                   stone include dirs (except the SDK's own), the pinned kernel
//! ```
//!
//! ponytail: the SDK itself is not bundled (installed from the lock, needs
//! the package feed); bundle prebuilt SDKs for offline sites later.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_info, print_success, OutputLevel};

pub const FORMAT: u32 = 1;
/// Where the container stages the payload, relative to the project dir.
pub const PAYLOAD_STAGE: &str = ".avocado/bundle-payload.tar.gz";
/// Where `--bundle` unpacks, relative to the working directory.
const UNPACK_DIR: &str = ".avocado-bundle";

/// Env vars that describe the packing host, not the bundle.
const HOST_ONLY_ENV: &[&str] = &[
    "AVOCADO_AGENT_SOCK",
    "AVOCADO_PROVISION_OUT",
    "AVOCADO_PROVISION_STATE",
    "AVOCADO_PROVISION_PROFILE",
    "AVOCADO_USB_PASSTHROUGH",
    "AVOCADO_VERBOSE",
];

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct BundleMeta {
    pub format: u32,
    pub target: String,
    pub runtime: String,
    /// `std::env::consts::ARCH` of the packing host: the SDK path in `env`.
    pub sdk_host_arch: String,
    pub env: HashMap<String, String>,
    pub payload_sha256: String,
    pub created_at: String,
    pub cli_version: String,
}

/// The shell that replaces the hook call when packing. Runs in the SDK
/// container after the var image is rendered. Fails closed: the runtime
/// shell has no `set -e`.
pub fn pack_tail(runtime: &str, target: &str) -> String {
    format!(
        r#"
P="$AVOCADO_PREFIX"
cd "$P" || exit 1
paths="output/runtimes/{runtime}/stone runtimes/{runtime}/avocado-image-var-{target}.btrfs"
IFS=':'
for inc in $AVOCADO_STONE_INCLUDE_PATHS; do
    [ -n "$inc" ] || continue
    case "$inc" in
        "$P"/sdk/*) ;;  # installed by `avocado sdk install` on the provisioning host
        "$P"/*) [ -e "$inc" ] && paths="$paths ${{inc#"$P"/}}" ;;
        *) echo "[ERROR] stone include path $inc is outside \$AVOCADO_PREFIX; it cannot be bundled" >&2; exit 1 ;;
    esac
done
unset IFS
if [ -n "$AVOCADO_PROVISION_KERNEL_IMAGE" ]; then
    k="$(dirname "$AVOCADO_PROVISION_KERNEL_IMAGE")"
    case "$k" in "$P"/*) paths="$paths ${{k#"$P"/}}" ;; esac
fi
mkdir -p /opt/src/.avocado || exit 1
echo "[INFO] Packing provisioning payload: $paths"
tar czf /opt/src/{PAYLOAD_STAGE} --exclude="output/runtimes/{runtime}/stone/_build" $paths || exit 1
"#
    )
}

/// The hook invocation `pack_tail` replaces (kept in one place so the two
/// can't drift: provision.rs builds its script with this).
pub fn hook_call(runtime: &str, target: &str) -> String {
    format!(
        "echo -e \"\\033[94m[INFO]\\033[0m Running SDK lifecycle hook 'avocado-provision' for '{runtime}'.\"\n\
         avocado-provision-{target} {runtime}\n"
    )
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    let mut hex = String::with_capacity(64);
    for byte in h.finalize().iter() {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

/// Write the bundle after the container staged the payload.
pub fn assemble(
    out: &Path,
    src_dir: &Path,
    config_path: &Path,
    target: &str,
    runtime: &str,
    env: &HashMap<String, String>,
) -> Result<()> {
    let payload = src_dir.join(PAYLOAD_STAGE);
    let lock = src_dir.join("avocado.lock");
    if !lock.exists() {
        bail!(
            "{} not found: run `avocado install` before packing",
            lock.display()
        );
    }
    let mut env = env.clone();
    env.retain(|k, _| !HOST_ONLY_ENV.contains(&k.as_str()));
    let meta = BundleMeta {
        format: FORMAT,
        target: target.into(),
        runtime: runtime.into(),
        sdk_host_arch: std::env::consts::ARCH.into(),
        env,
        payload_sha256: sha256_file(&payload)?,
        created_at: chrono::Utc::now().to_rfc3339(),
        cli_version: env!("AVOCADO_CLI_VERSION").into(),
    };
    let tmp = out.with_extension("partial");
    {
        let mut b = tar::Builder::new(std::fs::File::create(&tmp)?);
        let json = serde_json::to_vec_pretty(&meta)?;
        let mut h = tar::Header::new_gnu();
        h.set_size(json.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, "bundle.json", json.as_slice())?;
        b.append_path_with_name(config_path, "avocado.yaml")?;
        b.append_path_with_name(&lock, "avocado.lock")?;
        b.append_path_with_name(&payload, "payload.tar.gz")?;
        b.into_inner()?.flush()?;
    }
    std::fs::rename(&tmp, out)?;
    let _ = std::fs::remove_file(&payload);
    print_success(
        &format!(
            "Bundle written: {} ({} MiB, sha256 {})",
            out.display(),
            std::fs::metadata(out)?.len() >> 20,
            sha256_file(out)?
        ),
        OutputLevel::Normal,
    );
    Ok(())
}

/// Rewrite the packing host's SDK path (`…/sdk/<arch>/…`) to this host's.
pub fn localize_env(meta: &BundleMeta) -> HashMap<String, String> {
    let from = format!("/sdk/{}/", sdk_arch_dir(&meta.sdk_host_arch));
    let to = format!("/sdk/{}/", sdk_arch_dir(std::env::consts::ARCH));
    meta.env
        .iter()
        .map(|(k, v)| (k.clone(), v.replace(&from, &to)))
        .collect()
}

fn sdk_arch_dir(arch: &str) -> &str {
    match arch {
        "aarch64" | "arm64" => "aarch64",
        _ => "x86_64",
    }
}

async fn fetch(src: &str, dest: &Path) -> Result<()> {
    if src.starts_with("http://") || src.starts_with("https://") {
        print_info("Downloading bundle…", OutputLevel::Normal);
        let mut res = reqwest::get(src).await.context("bundle download")?;
        if !res.status().is_success() {
            bail!("bundle download failed: HTTP {}", res.status());
        }
        let mut f = std::fs::File::create(dest)?;
        while let Some(chunk) = res.chunk().await? {
            f.write_all(&chunk)?;
        }
    } else {
        std::fs::copy(src, dest).with_context(|| format!("copy {src}"))?;
    }
    Ok(())
}

pub struct BundleProvision {
    pub source: String,
    pub sha256: Option<String>,
    pub profile: Option<String>,
    pub env_vars: Option<HashMap<String, String>>,
    pub verbose: bool,
}

impl BundleProvision {
    pub async fn execute(&self) -> Result<()> {
        let cwd = std::env::current_dir()?;
        let dir = cwd.join(UNPACK_DIR);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let file = dir.join("bundle.tar");
        fetch(&self.source, &file).await?;
        if let Some(want) = &self.sha256 {
            let got = sha256_file(&file)?;
            if !got.eq_ignore_ascii_case(want) {
                bail!("bundle sha256 mismatch: expected {want}, got {got}");
            }
        }
        tar::Archive::new(std::fs::File::open(&file)?)
            .unpack(&dir)
            .context("unpack bundle")?;
        let meta: BundleMeta = serde_json::from_slice(&std::fs::read(dir.join("bundle.json"))?)
            .context("bundle.json")?;
        if meta.format != FORMAT {
            bail!(
                "unsupported bundle format {} (this CLI reads {FORMAT})",
                meta.format
            );
        }
        if sha256_file(&dir.join("payload.tar.gz"))? != meta.payload_sha256 {
            bail!("bundle payload is corrupt (sha256 mismatch)");
        }
        std::fs::copy(dir.join("avocado.yaml"), cwd.join("avocado.yaml"))?;
        std::fs::copy(dir.join("avocado.lock"), cwd.join("avocado.lock"))?;
        print_info(
            &format!(
                "Bundle: runtime '{}' for {} (packed {} by avocado {})",
                meta.runtime, meta.target, meta.created_at, meta.cli_version
            ),
            OutputLevel::Normal,
        );

        // The SDK matching the bundled lock. Reuses this directory's volume,
        // so later bundles for the same target skip the download.
        let exe = std::env::current_exe()?;
        let st = std::process::Command::new(exe)
            .args(["sdk", "install", "--no-tui", "-t", &meta.target])
            .status()
            .context("run avocado sdk install")?;
        if !st.success() {
            bail!("avocado sdk install failed ({st})");
        }

        let composed =
            crate::utils::config::Config::load_composed("avocado.yaml", Some(&meta.target))?;
        let config = &composed.config;
        let image = config
            .get_sdk_image()
            .context("No SDK container image specified in the bundled configuration")?;

        let mut env = localize_env(&meta);
        env.extend(self.env_vars.clone().unwrap_or_default());
        env.insert("AVOCADO_USB_PASSTHROUGH".into(), "1".into());
        if Path::new("/run/avocado-agent.sock").exists() {
            env.insert(
                "AVOCADO_AGENT_SOCK".into(),
                "/run/avocado-agent.sock".into(),
            );
        }
        if self.verbose {
            env.insert("AVOCADO_VERBOSE".into(), "1".into());
        }
        let rt = &meta.runtime;
        let script = format!(
            r#"
cd "$AVOCADO_PREFIX" || exit 1
rm -rf "output/runtimes/{rt}/stone" || exit 1
tar xzf /opt/src/{UNPACK_DIR}/payload.tar.gz || exit 1
{hook}"#,
            hook = hook_call(rt, &meta.target),
        );
        let ok = SdkContainer::new()
            .run_in_container(RunConfig {
                container_image: image.to_string(),
                target: meta.target.clone(),
                command: script,
                verbose: self.verbose,
                source_environment: true,
                interactive: false,
                env_vars: Some(env),
                container_args: config
                    .merge_provision_container_args(self.profile.as_deref(), None),
                ..Default::default()
            })
            .await
            .context("Failed to provision from bundle")?;
        let _ = std::fs::remove_file(&file);
        if !ok {
            bail!("Failed to provision runtime '{rt}' from bundle");
        }
        print_success(
            &format!("Successfully provisioned runtime '{rt}' from bundle"),
            OutputLevel::Normal,
        );
        Ok(())
    }
}

/// Default output name for `--pack` when given a directory.
pub fn default_out(dir: &Path, runtime: &str, target: &str) -> PathBuf {
    dir.join(format!("{runtime}-{target}.avocado-bundle.tar"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(arch: &str) -> BundleMeta {
        BundleMeta {
            format: FORMAT,
            target: "jetson-orin-nano".into(),
            runtime: "dev".into(),
            sdk_host_arch: arch.into(),
            env: HashMap::from([(
                "AVOCADO_STONE_INCLUDE_PATHS".into(),
                format!("/opt/_avocado/t/sdk/{arch}/stone:/opt/_avocado/t/includes/bsp/stone"),
            )]),
            payload_sha256: String::new(),
            created_at: String::new(),
            cli_version: String::new(),
        }
    }

    #[test]
    fn localize_rewrites_only_the_sdk_arch() {
        let env = localize_env(&meta("aarch64"));
        let here = sdk_arch_dir(std::env::consts::ARCH);
        assert_eq!(
            env["AVOCADO_STONE_INCLUDE_PATHS"],
            format!("/opt/_avocado/t/sdk/{here}/stone:/opt/_avocado/t/includes/bsp/stone")
        );
    }

    #[test]
    fn pack_tail_replaces_exactly_the_hook_call() {
        let tail = pack_tail("dev", "jetson-orin-nano");
        assert!(!tail.contains(&hook_call("dev", "jetson-orin-nano")));
        assert!(tail.contains(PAYLOAD_STAGE));
        assert!(tail.contains("|| exit 1"));
    }

    #[test]
    fn pack_tail_is_valid_sh() {
        let st = std::process::Command::new("sh")
            .args(["-n", "-c", &pack_tail("dev", "jetson-orin-nano")])
            .status()
            .unwrap();
        assert!(st.success());
    }
}

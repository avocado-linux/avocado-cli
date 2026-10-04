//! Provisionable runtime bundles, content-addressed.
//!
//! `avocado provision --pack <dir>` runs provisioning up to, not including,
//! the SDK's `avocado-provision-<target>` hook and saves what that hook
//! needs as a bundle directory:
//!
//! ```text
//! <runtime>-<target>.bundle/
//!   bundle.json       target, runtime, env, avocado.yaml/lock, the provision
//!                     script to replay, and a manifest of files by sha256
//!   blobs/<sha256>    each distinct large file (>= 1 MiB) and every runtime
//!                     image (`lib/avocado/images/*`) once, plus a
//!                     deterministic tar.gz of the small files under each
//!                     top-level path (so per-build metadata in var-staging
//!                     doesn't re-key the static BSP and flash tools)
//! ```
//!
//! The rendered var image is not shipped: `var-staging` is (captured after
//! Docker priming), and the replay renders the image from it. Its large
//! files are the runtime's images (`lib/avocado/images/<image_id>.raw`),
//! the same bytes `avocado connect upload` stores as OTA artifacts, so
//! Connect stores each once whichever upload brings it first.
//!
//! `avocado provision --bundle <spec.json | bundle dir>` replays it on
//! another machine (an on-site VM hub) with only the SDK installed: no build
//! tree, no extension sysroots. Blobs are cached by sha256; a repeat job
//! fetches nothing.
//!
//! ponytail: the SDK itself is not bundled (installed from the lock, needs
//! the package feed); bundle prebuilt SDKs for offline sites later.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_info, print_success, OutputLevel};

pub const FORMAT: u32 = 2;
/// Files at least this big are stored as their own blob.
pub const LARGE_FILE: u64 = 1 << 20;
/// Where the container stages a pack, relative to the project dir.
pub const STAGE_DIR: &str = ".avocado/bundle-stage";
/// Where `--bundle` keeps blobs and its working files, relative to the cwd.
const WORK_DIR: &str = ".avocado-bundle";

/// Env vars that describe the packing host, not the bundle.
const HOST_ONLY_ENV: &[&str] = &[
    "AVOCADO_AGENT_SOCK",
    "AVOCADO_PROVISION_OUT",
    "AVOCADO_PROVISION_STATE",
    "AVOCADO_PROVISION_PROFILE",
    "AVOCADO_USB_PASSTHROUGH",
    "AVOCADO_VERBOSE",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileEntry {
    /// Relative to `$AVOCADO_PREFIX`.
    pub path: String,
    pub sha256: String,
    pub size: u64,
    /// Octal permission bits, as `stat -c %a` prints them.
    pub mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BlobRef {
    pub sha256: String,
    pub size: u64,
}

/// The small files, symlinks and directories under one top-level path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SmallArchive {
    pub root: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct BundleMeta {
    pub format: u32,
    pub target: String,
    pub runtime: String,
    /// `std::env::consts::ARCH` of the packing host: the SDK path in `env`.
    pub sdk_host_arch: String,
    pub env: HashMap<String, String>,
    pub avocado_yaml: String,
    pub avocado_lock: String,
    /// What `avocado provision` would run after its build half: render var
    /// from var-staging, refresh it into stone/, run the hook.
    pub provision_script: String,
    /// Top-level paths under $AVOCADO_PREFIX the bundle replaces; cleared
    /// before restoring so an earlier bundle's files can't shadow these.
    pub clear: Vec<String>,
    pub files: Vec<FileEntry>,
    pub small_files: Vec<SmallArchive>,
    pub created_at: String,
    pub cli_version: String,
}

impl BundleMeta {
    /// Every distinct blob the bundle needs (large files, then the small
    /// files archive), each once.
    pub fn blobs(&self) -> Vec<BlobRef> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for b in self
            .files
            .iter()
            .map(|f| BlobRef {
                sha256: f.sha256.clone(),
                size: f.size,
            })
            .chain(self.small_files.iter().map(|a| BlobRef {
                sha256: a.sha256.clone(),
                size: a.size,
            }))
        {
            if seen.insert(b.sha256.clone()) {
                out.push(b);
            }
        }
        out
    }

    pub fn total_bytes(&self) -> u64 {
        self.blobs().iter().map(|b| b.size).sum()
    }
}

/// The shell that replaces the hook call when packing. Runs in the SDK
/// container after the var section ran (var-staging holds the primed Docker
/// images). Stages `files.tsv` (large files by sha256), `blobs/`, and the
/// small-files archive under /opt/src/STAGE_DIR. Fails closed: the runtime
/// shell has no `set -e`.
pub fn pack_tail(runtime: &str, target: &str) -> String {
    format!(
        r#"
P="$AVOCADO_PREFIX"
cd "$P" || exit 1
paths="output/runtimes/{runtime}/stone runtimes/{runtime}/var-staging"
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
    case "$k" in
        "$P"/*) paths="$paths ${{k#"$P"/}}" ;;
        *) echo "[ERROR] kernel image $AVOCADO_PROVISION_KERNEL_IMAGE is outside \$AVOCADO_PREFIX; it cannot be bundled" >&2; exit 1 ;;
    esac
fi
if [ -n "$AVOCADO_STONE_MANIFEST" ]; then
    case "$AVOCADO_STONE_MANIFEST" in
        "$P"/sdk/*) ;;
        "$P"/*) paths="$paths ${{AVOCADO_STONE_MANIFEST#"$P"/}}" ;;
        *) echo "[ERROR] stone manifest $AVOCADO_STONE_MANIFEST is outside \$AVOCADO_PREFIX; it cannot be bundled" >&2; exit 1 ;;
    esac
fi
OUT=/opt/src/{STAGE_DIR}
rm -rf "$OUT" && mkdir -p "$OUT/blobs" || exit 1
echo "$paths" > "$OUT/paths" || exit 1
# Never bundled: the build scratch dir, and the rendered var image (the
# replay renders it from var-staging).
SKIP="-path output/runtimes/{runtime}/stone/_build -o -path output/runtimes/{runtime}/stone/avocado-image-var-{target}.btrfs"
# Own blobs: anything large, and every runtime image whatever its size (they
# are the files `connect upload` stores as OTA artifacts, so both uploads
# reuse each other's content).
BLOB="-size +{small_max}c -o -path runtimes/{runtime}/var-staging/lib/avocado/images/*"
set -f  # $BLOB holds a find pattern: don't let the shell glob it
echo "[INFO] Packing provisioning bundle from: $paths"
: > "$OUT/files.tsv" || exit 1
find $paths \( $SKIP \) -prune -o -type f \( $BLOB \) -print | LC_ALL=C sort | while IFS= read -r f; do
    h=$(sha256sum "$f" | cut -d' ' -f1) || exit 1
    [ -e "$OUT/blobs/$h" ] || cp "$f" "$OUT/blobs/$h" || exit 1
    printf '%s\t%s\t%s\t%s\n' "$h" "$(stat -c %s "$f")" "$(stat -c %a "$f")" "$f" >> "$OUT/files.tsv" || exit 1
done || exit 1
# Everything else (small files, symlinks, directories), one archive per
# top-level path, byte-identical for identical content so each deduplicates.
: > "$OUT/small.tsv" || exit 1
for p in $paths; do
    find "$p" \( $SKIP \) -prune -o -type f \( $BLOB \) -o -print | LC_ALL=C sort > "$OUT/small.list" || exit 1
    tar --no-recursion --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner \
        -cf - -T "$OUT/small.list" | gzip -n > "$OUT/small.tar.gz" || exit 1
    h=$(sha256sum "$OUT/small.tar.gz" | cut -d' ' -f1) || exit 1
    printf '%s\t%s\t%s\n' "$h" "$(stat -c %s "$OUT/small.tar.gz")" "$p" >> "$OUT/small.tsv" || exit 1
    mv "$OUT/small.tar.gz" "$OUT/blobs/$h" || exit 1
done
set +f
"#,
        small_max = LARGE_FILE - 1,
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

pub fn sha256_file(path: &Path) -> Result<String> {
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

/// Parse the container's `files.tsv` (sha, size, mode, path per line).
pub fn parse_files_tsv(tsv: &str) -> Result<Vec<FileEntry>> {
    tsv.lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let mut c = l.splitn(4, '\t');
            let (Some(sha), Some(size), Some(mode), Some(path)) =
                (c.next(), c.next(), c.next(), c.next())
            else {
                bail!("malformed files.tsv line: {l:?}");
            };
            Ok(FileEntry {
                path: path.to_string(),
                sha256: sha.to_string(),
                size: size.parse()?,
                mode: mode.to_string(),
            })
        })
        .collect()
}

pub struct PackInputs<'a> {
    pub src_dir: &'a Path,
    pub config_path: &'a Path,
    pub target: &'a str,
    pub runtime: &'a str,
    pub env: &'a HashMap<String, String>,
    pub provision_script: String,
}

/// Turn the container's staging into `<out>/<runtime>-<target>.bundle/`.
pub fn assemble(out: &Path, p: PackInputs) -> Result<PathBuf> {
    let stage = p.src_dir.join(STAGE_DIR);
    let lock = p.src_dir.join("avocado.lock");
    if !lock.exists() {
        bail!(
            "{} not found: run `avocado install` before packing",
            lock.display()
        );
    }
    let mut env = p.env.clone();
    env.retain(|k, _| !HOST_ONLY_ENV.contains(&k.as_str()));
    let files = parse_files_tsv(&std::fs::read_to_string(stage.join("files.tsv"))?)?;
    let small_files = parse_files_tsv(
        &std::fs::read_to_string(stage.join("small.tsv"))?
            .lines()
            .map(|l| {
                // sha, size, root → the 4-column shape (mode unused)
                let mut c = l.splitn(3, '\t');
                format!(
                    "{}\t{}\t-\t{}\n",
                    c.next().unwrap_or(""),
                    c.next().unwrap_or(""),
                    c.next().unwrap_or("")
                )
            })
            .collect::<String>(),
    )?
    .into_iter()
    .map(|f| SmallArchive {
        root: f.path,
        sha256: f.sha256,
        size: f.size,
    })
    .collect();
    let meta = BundleMeta {
        format: FORMAT,
        target: p.target.into(),
        runtime: p.runtime.into(),
        sdk_host_arch: std::env::consts::ARCH.into(),
        env,
        avocado_yaml: std::fs::read_to_string(p.config_path)?,
        avocado_lock: std::fs::read_to_string(&lock)?,
        provision_script: p.provision_script,
        clear: std::fs::read_to_string(stage.join("paths"))?
            .split_whitespace()
            .map(String::from)
            .collect(),
        files,
        small_files,
        created_at: chrono::Utc::now().to_rfc3339(),
        cli_version: env!("AVOCADO_CLI_VERSION").into(),
    };
    let dir = if out.is_dir() {
        out.join(format!("{}-{}.bundle", p.runtime, p.target))
    } else {
        out.to_path_buf()
    };
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("bundle.json"), serde_json::to_vec_pretty(&meta)?)?;
    let blobs = dir.join("blobs");
    if std::fs::rename(stage.join("blobs"), &blobs).is_err() {
        // Different filesystem: copy, then drop the staging copy.
        std::fs::create_dir_all(&blobs)?;
        for e in std::fs::read_dir(stage.join("blobs"))? {
            let e = e?;
            std::fs::copy(e.path(), blobs.join(e.file_name()))?;
        }
    }
    let _ = std::fs::remove_dir_all(&stage);
    let file_bytes: u64 = meta.files.iter().map(|f| f.size).sum();
    print_success(
        &format!(
            "Bundle written: {} ({} files referenced, {} MiB in {} distinct blobs; \
             {} MiB of large files before dedupe)",
            dir.display(),
            meta.files.len(),
            meta.total_bytes() >> 20,
            meta.blobs().len(),
            file_bytes >> 20,
        ),
        OutputLevel::Normal,
    );
    Ok(dir)
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

/// A relative path below the prefix that is safe to `rm -rf` in quotes.
fn safe_rel(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && p.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
        && p.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-+/".contains(c))
}

/// A manifest file path: relative, no `..`, nothing that breaks the TSV.
fn plain_rel(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && p.split('/').all(|c| !c.is_empty() && c != "..")
        && !p.chars().any(|c| c.is_control())
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn sdk_arch_dir(arch: &str) -> &str {
    match arch {
        "aarch64" | "arm64" => "aarch64",
        _ => "x86_64",
    }
}

/// What a job hands `--bundle`: the bundle.json plus where each blob is.
#[derive(Debug, Deserialize)]
pub struct BundleSpec {
    pub bundle_json: BundleMeta,
    pub blobs: Vec<SpecBlob>,
}

#[derive(Debug, Deserialize)]
pub struct SpecBlob {
    pub sha256: String,
    pub url: String,
}

/// Put blob `sha` into the cache from `src` (URL or local file), verified.
async fn fetch_blob(src: &str, sha: &str, cache: &Path) -> Result<()> {
    let dest = cache.join(sha);
    if dest.exists() {
        return Ok(());
    }
    let tmp = cache.join(format!(".{sha}.part"));
    if src.starts_with("http://") || src.starts_with("https://") {
        let mut res = reqwest::get(src).await.context("blob download")?;
        if !res.status().is_success() {
            bail!("blob {sha} download failed: HTTP {}", res.status());
        }
        let mut f = std::fs::File::create(&tmp)?;
        while let Some(chunk) = res.chunk().await? {
            f.write_all(&chunk)?;
        }
    } else {
        std::fs::copy(src, &tmp).with_context(|| format!("copy {src}"))?;
    }
    let got = sha256_file(&tmp)?;
    if got != sha {
        let _ = std::fs::remove_file(&tmp);
        bail!("blob {sha} is corrupt (got sha256 {got})");
    }
    std::fs::rename(&tmp, &dest)?;
    Ok(())
}

pub struct BundleProvision {
    /// A spec.json from a job, or a local bundle directory from `--pack`.
    pub source: String,
    pub profile: Option<String>,
    pub env_vars: Option<HashMap<String, String>>,
    pub verbose: bool,
}

impl BundleProvision {
    pub async fn execute(&self) -> Result<()> {
        let src = Path::new(&self.source);
        let (meta, locations): (BundleMeta, HashMap<String, String>) = if src.is_dir() {
            let meta: BundleMeta = serde_json::from_slice(&std::fs::read(src.join("bundle.json"))?)
                .context("bundle.json")?;
            let loc = meta
                .blobs()
                .into_iter()
                .map(|b| {
                    let p = src
                        .join("blobs")
                        .join(&b.sha256)
                        .to_string_lossy()
                        .into_owned();
                    (b.sha256, p)
                })
                .collect();
            (meta, loc)
        } else {
            let spec: BundleSpec = serde_json::from_slice(&std::fs::read(src)?)
                .with_context(|| format!("{} is not a bundle spec", src.display()))?;
            let loc = spec.blobs.into_iter().map(|b| (b.sha256, b.url)).collect();
            (spec.bundle_json, loc)
        };
        if meta.format != FORMAT {
            bail!(
                "unsupported bundle format {} (this CLI reads {FORMAT})",
                meta.format
            );
        }
        for f in &meta.files {
            if !plain_rel(&f.path)
                || !is_sha256(&f.sha256)
                || !f.mode.bytes().all(|b| (b'0'..=b'7').contains(&b))
            {
                bail!("bundle manifest entry {:?} is not valid", f.path);
            }
        }
        if meta.small_files.iter().any(|a| !is_sha256(&a.sha256)) {
            bail!("bundle small-file archive entry is not valid");
        }
        if let Some(bad) = meta.clear.iter().find(|p| !safe_rel(p)) {
            bail!("bundle path {bad:?} is not a plain relative path");
        }
        print_info(
            &format!(
                "Bundle: runtime '{}' for {} (packed {} by avocado {})",
                meta.runtime, meta.target, meta.created_at, meta.cli_version
            ),
            OutputLevel::Normal,
        );

        // Blob cache: fetch only what isn't here yet, keep only what this
        // bundle uses (bounds the disk to one bundle's worth).
        let cwd = std::env::current_dir()?;
        let work = cwd.join(WORK_DIR);
        let cache = work.join("blobs");
        std::fs::create_dir_all(&cache)?;
        let needed = meta.blobs();
        let (mut fetched, mut fetched_bytes) = (0, 0u64);
        for b in &needed {
            if cache.join(&b.sha256).exists() {
                continue;
            }
            let loc = locations
                .get(&b.sha256)
                .with_context(|| format!("no location for blob {}", b.sha256))?;
            fetch_blob(loc, &b.sha256, &cache).await?;
            fetched += 1;
            fetched_bytes += b.size;
        }
        print_info(
            &format!(
                "Blobs: {} needed, {fetched} fetched ({} MiB), {} cached",
                needed.len(),
                fetched_bytes >> 20,
                needed.len() - fetched
            ),
            OutputLevel::Normal,
        );
        let keep: HashSet<&str> = needed.iter().map(|b| b.sha256.as_str()).collect();
        for e in std::fs::read_dir(&cache)?.flatten() {
            if !keep.contains(&*e.file_name().to_string_lossy()) {
                let _ = std::fs::remove_file(e.path());
            }
        }

        std::fs::write(cwd.join("avocado.yaml"), &meta.avocado_yaml)?;
        std::fs::write(cwd.join("avocado.lock"), &meta.avocado_lock)?;
        let tsv: String = meta
            .files
            .iter()
            .map(|f| format!("{}\t{}\t{}\n", f.sha256, f.mode, f.path))
            .collect();
        std::fs::write(work.join("files.tsv"), tsv)?;

        // The SDK matching the bundled lock. Reuses this directory's volume,
        // so later bundles for the same target skip the download.
        let exe = std::env::current_exe()?;
        let st = std::process::Command::new(exe)
            .args(["sdk", "install", "--no-tui", "--target", &meta.target])
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
        let clear: String = meta
            .clear
            .iter()
            .map(|p| format!("rm -rf '{p}' || exit 1\n"))
            .collect();
        let small: String = meta
            .small_files
            .iter()
            .map(|a| format!("tar xzf \"$B/blobs/{}\" || exit 1\n", a.sha256))
            .collect();
        let script = format!(
            r#"
cd "$AVOCADO_PREFIX" || exit 1
B=/opt/src/{WORK_DIR}
{clear}{small}while IFS=$'\t' read -r h mode path; do
    mkdir -p "$(dirname "$path")" && cp "$B/blobs/$h" "$path" && chmod "$mode" "$path" || exit 1
done < "$B/files.tsv" || exit 1
echo "[INFO] Restored the bundle into $AVOCADO_PREFIX"
{provision}"#,
            provision = meta.provision_script,
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
        if !ok {
            bail!("Failed to provision runtime '{}' from bundle", meta.runtime);
        }
        print_success(
            &format!(
                "Successfully provisioned runtime '{}' from bundle",
                meta.runtime
            ),
            OutputLevel::Normal,
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

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
            avocado_yaml: String::new(),
            avocado_lock: String::new(),
            provision_script: String::new(),
            clear: vec![],
            files: vec![
                FileEntry {
                    path: "output/runtimes/dev/stone/rootfs.erofs".into(),
                    sha256: A.into(),
                    size: 10,
                    mode: "644".into(),
                },
                FileEntry {
                    path: "runtimes/dev/var-staging/lib/avocado/images/x.raw".into(),
                    sha256: A.into(),
                    size: 10,
                    mode: "644".into(),
                },
                FileEntry {
                    path: "output/runtimes/dev/stone/esp.img".into(),
                    sha256: B.into(),
                    size: 5,
                    mode: "644".into(),
                },
            ],
            small_files: vec![SmallArchive {
                root: "output/runtimes/dev/stone".into(),
                sha256: "c".repeat(64),
                size: 1,
            }],
            created_at: String::new(),
            cli_version: String::new(),
        }
    }

    #[test]
    fn identical_files_are_one_blob() {
        let m = meta("x86_64");
        let blobs = m.blobs();
        assert_eq!(blobs.len(), 3, "A once, B once, the small-files archive");
        assert_eq!(m.total_bytes(), 10 + 5 + 1);
    }

    #[test]
    fn files_tsv_round_trip() {
        let f = parse_files_tsv(&format!(
            "{A}\t10\t755\tkernel/6.18/Image\n{B}\t5\t644\ta b/c\n"
        ))
        .unwrap();
        assert_eq!(f[0].path, "kernel/6.18/Image");
        assert_eq!(f[0].mode, "755");
        assert_eq!(f[1].path, "a b/c");
        assert!(parse_files_tsv("nope").is_err());
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
    fn manifest_paths_are_validated() {
        assert!(plain_rel("runtimes/dev/var-staging/lib/docker/a b"));
        for bad in ["", "/etc/x", "../x", "a/../b", "a\tb", "a\nb"] {
            assert!(!plain_rel(bad), "{bad:?}");
        }
        assert!(safe_rel("output/runtimes/dev/stone"));
        for bad in ["", "/etc", "../x", "a//b", "a b", "a'b", "a;rm"] {
            assert!(!safe_rel(bad), "{bad:?}");
        }
    }

    #[test]
    fn pack_tail_is_valid_sh_and_fails_closed() {
        let tail = pack_tail("dev", "jetson-orin-nano");
        assert!(!tail.contains(&hook_call("dev", "jetson-orin-nano")));
        assert!(tail.contains("|| exit 1"));
        let st = std::process::Command::new("sh")
            .args(["-n", "-c", &tail])
            .status()
            .unwrap();
        assert!(st.success());
    }

    /// The pack's split must be exact: every regular file is large xor small.
    #[test]
    fn large_and_small_partition_the_tree() {
        let dir = std::env::temp_dir().join(format!("bundle-split-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("t/sub")).unwrap();
        std::fs::write(dir.join("t/big"), vec![0u8; LARGE_FILE as usize]).unwrap();
        std::fs::write(dir.join("t/sub/small"), vec![0u8; LARGE_FILE as usize - 1]).unwrap();
        let run = |extra: &str| {
            let out = std::process::Command::new("sh")
                .current_dir(&dir)
                .args([
                    "-c",
                    &format!("find t -type f -size +{}c {extra} | sort", LARGE_FILE - 1),
                ])
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap()
        };
        assert_eq!(run("-print"), "t/big\n");
        let small = std::process::Command::new("sh")
            .current_dir(&dir)
            .args([
                "-c",
                &format!("find t -type f -size +{}c -o -print | sort", LARGE_FILE - 1),
            ])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(small.stdout).unwrap(),
            "t\nt/sub\nt/sub/small\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

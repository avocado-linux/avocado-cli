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
//! Extensions whose source is a path on the build site travel in the bundle
//! too (`ext_sources`, one deterministic tar.gz each), and its avocado.yaml
//! points them at the copies: a hub has none of the build site's paths, and
//! `sdk install` there merges each extension's config before anything else.
//!
//! ponytail: the SDK itself is not bundled (installed from the lock, needs
//! the package feed); bundle prebuilt SDKs for offline sites later. Git
//! sources are still fetched by the hub (public repos only); private git
//! and private feeds need the same carry-or-credential answer
//! (docs/bundles.md).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::utils::config::{ComposedConfig, Config, ProvisionRubicon, RubiconStep};
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_info, print_success, OutputLevel};

pub const FORMAT: u32 = 2;
/// Files at least this big are stored as their own blob.
pub const LARGE_FILE: u64 = 1 << 20;
/// Where the container stages a pack, relative to the project dir: one per
/// runtime and target, so packs of different runtimes from one project
/// (side by side, to different boards) don't clear each other's staging.
/// Two packs of the same runtime already share its build output.
pub fn stage_dir(runtime: &str, target: &str) -> String {
    format!(".avocado/bundle-stage/{runtime}-{target}")
}
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
pub struct ExtSourceArchive {
    /// The extension's name (resolved: no templates).
    pub name: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
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
    /// What a hub runs on the Rubicon around this provision, and how it reads
    /// the console; absent when the config has no hooks, wiring or console.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rubicon: Option<BundleRubicon>,
    /// Path-sourced extensions, each a tar.gz of its directory; the bundled
    /// avocado.yaml points them at `.avocado-bundle/ext/<name>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ext_sources: Vec<ExtSourceArchive>,
}

/// The Rubicon side of a bundle, resolved from the composed config at pack
/// time so the hub needs no config of its own: a job's profile (else
/// `default_profile`) picks an entry of `profiles`, whose names index `run`.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct BundleRubicon {
    /// Stone's default provision profile, used when the job names none.
    pub default_profile: Option<String>,
    pub profiles: BTreeMap<String, ProvisionRubicon>,
    pub run: BTreeMap<String, Vec<RubiconStep>>,
    /// `rubicon.wiring`: what the board's pins are wired to, for the log.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub wiring: BTreeMap<String, String>,
    /// `rubicon.console`: the rate and port the hub reads the console at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub console: Option<crate::utils::config::RubiconConsole>,
}

/// The run profiles the config's provision profiles reference, by profile.
pub fn bundle_rubicon(
    config: &Config,
    default_profile: Option<String>,
) -> Result<Option<BundleRubicon>> {
    config.check_rubicon_refs()?;
    let profiles: BTreeMap<String, ProvisionRubicon> = config
        .provision_profiles
        .iter()
        .flatten()
        .filter_map(|(name, p)| Some((name.clone(), p.rubicon.clone()?)))
        .collect();
    let wiring = config
        .rubicon
        .as_ref()
        .map(|r| r.wiring.clone())
        .unwrap_or_default();
    let console = config.rubicon.as_ref().and_then(|r| r.console.clone());
    // Console settings and wiring are for the hub whether or not a profile
    // has hooks (none: nothing runs around the provision).
    if profiles.is_empty() && wiring.is_empty() && console.is_none() {
        return Ok(None);
    }
    // Flattened (`run:` steps expanded): the hub runs plain steps.
    let run = profiles
        .values()
        .flat_map(|r| [&r.before, &r.after])
        .flatten()
        .map(|name| Ok((name.clone(), config.rubicon_steps(name)?)))
        .collect::<Result<_>>()?;
    Ok(Some(BundleRubicon {
        default_profile,
        profiles,
        run,
        wiring,
        console,
    }))
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
            .chain(self.ext_sources.iter().map(|a| BlobRef {
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
/// small-files archive under /opt/src/`stage_dir`. Fails closed: the runtime
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
OUT=/opt/src/{stage}
rm -rf "$OUT" && mkdir -p "$OUT/blobs" || exit 1
echo "$paths" > "$OUT/paths" || exit 1
# The stone manifest names the default provision profile (for the bundle's
# Rubicon run profiles); not bundled itself when it comes with the SDK.
m="${{AVOCADO_STONE_MANIFEST:-$AVOCADO_SDK_PREFIX/stone/stone-{target}.json}}"
if [ -f "$m" ]; then cp "$m" "$OUT/stone-manifest.json" || exit 1; fi
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
# Lists go through files, not pipes: a failing find must fail the pack, not
# leave a shorter list behind (the shell has no pipefail here).
find $paths \( $SKIP \) -prune -o -type f \( $BLOB \) -print > "$OUT/blobs.found" || exit 1
LC_ALL=C sort "$OUT/blobs.found" > "$OUT/blobs.list" || exit 1
while IFS= read -r f; do
    h=$(sha256sum "$f" | cut -d' ' -f1) || exit 1
    [ -e "$OUT/blobs/$h" ] || cp "$f" "$OUT/blobs/$h" || exit 1
    printf '%s\t%s\t%s\t%s\n' "$h" "$(stat -c %s "$f")" "$(stat -c %a "$f")" "$f" >> "$OUT/files.tsv" || exit 1
done < "$OUT/blobs.list" || exit 1
rm -f "$OUT/blobs.found" "$OUT/blobs.list"
# Everything else (small files, symlinks, directories), one archive per
# top-level path, byte-identical for identical content so each deduplicates.
: > "$OUT/small.tsv" || exit 1
for p in $paths; do
    find "$p" \( $SKIP \) -prune -o -type f \( $BLOB \) -o -print > "$OUT/small.found" || exit 1
    LC_ALL=C sort "$OUT/small.found" > "$OUT/small.list" || exit 1
    tar --no-recursion --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner \
        -cf "$OUT/small.tar" -T "$OUT/small.list" || exit 1
    gzip -n -f "$OUT/small.tar" || exit 1
    h=$(sha256sum "$OUT/small.tar.gz" | cut -d' ' -f1) || exit 1
    printf '%s\t%s\t%s\n' "$h" "$(stat -c %s "$OUT/small.tar.gz")" "$p" >> "$OUT/small.tsv" || exit 1
    mv "$OUT/small.tar.gz" "$OUT/blobs/$h" || exit 1
done
rm -f "$OUT/small.found" "$OUT/small.list"
set +f
"#,
        small_max = LARGE_FILE - 1,
        stage = stage_dir(runtime, target),
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
    pub composed: &'a ComposedConfig,
}

/// Turn the container's staging into `<out>/<runtime>-<target>.bundle/`.
pub fn assemble(out: &Path, p: PackInputs) -> Result<PathBuf> {
    let stage = p.src_dir.join(stage_dir(p.runtime, p.target));
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
    let (ext_sources, avocado_yaml) = pack_ext_sources(
        p.config_path,
        p.target,
        &required_extensions(p.composed, p.target)?,
        &stage.join("blobs"),
    )?;
    let meta = BundleMeta {
        format: FORMAT,
        target: p.target.into(),
        runtime: p.runtime.into(),
        sdk_host_arch: std::env::consts::ARCH.into(),
        env,
        avocado_yaml,
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
        rubicon: bundle_rubicon(
            &p.composed.config,
            std::fs::read_to_string(stage.join("stone-manifest.json"))
                .ok()
                .and_then(|m| crate::commands::profiles::stone_default_profile(&m)),
        )?,
        ext_sources,
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

/// The extensions the replay's `sdk install` uses for `target` (those its
/// runtimes list, as there), plus everything they depend on.
pub fn required_extensions(composed: &ComposedConfig, target: &str) -> Result<HashSet<String>> {
    let graph = crate::utils::ext_deps::DependencyGraph::from_composed(composed, target)?;
    let mut todo: Vec<String> = crate::utils::config::find_active_extensions(
        &composed.config,
        &composed.merged_value,
        target,
        &composed.config_path,
        None,
    )?
    .into_iter()
    .collect();
    let mut out = HashSet::new();
    while let Some(name) = todo.pop() {
        if let Some(node) = graph.get(&name).filter(|_| !out.contains(&name)) {
            todo.extend(node.depends_on.iter().map(|d| d.name.clone()));
        }
        out.insert(name);
    }
    Ok(out)
}

/// Archive each path-sourced extension in `required` into `blobs` and return
/// them with the config text the bundle carries, its path sources pointing
/// at the copies a hub unpacks (`.avocado-bundle/ext/<name>`) and its
/// path-sourced image sections inlined. Other targets' extensions stay as
/// written: absent on the hub, they are skipped like any unfetched source.
pub fn pack_ext_sources(
    config_path: &Path,
    target: &str,
    required: &HashSet<String>,
    blobs: &Path,
) -> Result<(Vec<ExtSourceArchive>, String)> {
    use crate::utils::config::ExtensionSource;
    use crate::utils::interpolation::{interpolate_config, preprocess_text, AvocadoContext};

    let text = std::fs::read_to_string(config_path)?;
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&text)?;
    // The document as the build resolved it: extension paths, `src_dir` and
    // the `{{ config.* }}` references in either are read from this.
    let mut resolved = doc.clone();
    interpolate_config(&mut resolved, Some(target), None)
        .with_context(|| format!("interpolate {}", config_path.display()))?;
    // A relative path source resolves against `src_dir` (itself relative to
    // the config's directory), else the config's directory.
    let config_dir = config_path.parent().unwrap_or(Path::new("."));
    let base = match resolved.get("src_dir").and_then(|v| v.as_str()) {
        Some(src_dir) => config_dir.join(src_dir),
        None => config_dir.to_path_buf(),
    };
    let found = Config::discover_remote_extensions_from_value(&resolved)?;
    // Source path as written -> the bundle-local path it becomes.
    let mut moved: HashMap<String, String> = HashMap::new();
    let mut out = Vec::new();
    for (name, source) in found {
        let ExtensionSource::Path { path, .. } = source else {
            continue;
        };
        if !required.contains(&name) {
            continue;
        }
        if !plain_name(&name) {
            bail!("extension name {name:?} cannot name a bundle directory");
        }
        let dir = base.join(&path);
        let archive = tar_dir(&dir)
            .with_context(|| format!("archive extension '{name}' from {}", dir.display()))?;
        let sha256 = hex_sha256(&archive);
        std::fs::create_dir_all(blobs)?;
        std::fs::write(blobs.join(&sha256), &archive)?;
        moved.insert(path, format!("{WORK_DIR}/ext/{name}"));
        out.push(ExtSourceArchive {
            name,
            sha256,
            size: archive.len() as u64,
        });
    }
    // `moved` is keyed by the resolved path; the raw text may still template
    // it (`../extensions/bsp-{{ avocado.target.board }}`,
    // `{{ config.ext_root }}/bsp`), so resolve each written path against the
    // resolved document before looking it up.
    let ctx = AvocadoContext::from_main_config(&doc, Some(target), None);
    if let Some(board) = &ctx.target_board {
        pin_board(&mut doc, board);
    }
    if let Some(exts) = doc.get_mut("extensions").and_then(|e| e.as_mapping_mut()) {
        for (_, ext) in exts.iter_mut() {
            let Some(src) = ext.get_mut("source").and_then(|s| s.as_mapping_mut()) else {
                continue;
            };
            if src.get("type").and_then(|t| t.as_str()) != Some("path") {
                continue;
            }
            let Some(written) = src.get("path").and_then(|p| p.as_str()) else {
                continue;
            };
            if let Some(to) = moved.get(&preprocess_text(written, &resolved, &ctx)?) {
                src.insert("path".into(), to.clone().into());
            }
        }
    }
    // `rootfs`/`initramfs`/`kernel` from a path fragment: its section goes
    // in over the declaration, as the build composed it (the fragment is not
    // on the hub).
    let mut merged = resolved.clone();
    Config::merge_path_based_image_sections(&mut merged, config_path)?;
    for key in ["rootfs", "initramfs", "kernel"] {
        let from_path = doc
            .get(key)
            .and_then(|s| s.get("source"))
            .and_then(|s| s.get("type"))
            .and_then(|t| t.as_str())
            == Some("path");
        if let (true, Some(section), Some(map)) = (from_path, merged.get(key), doc.as_mapping_mut())
        {
            map.insert(key.into(), section.clone());
        }
    }
    // The replay's directory is the project root: the carried copies above
    // and the lock sit beside its config, wherever `src_dir` pointed here.
    if let Some(map) = doc.as_mapping_mut() {
        map.remove("src_dir");
    }
    Ok((out, serde_yaml::to_string(&doc)?))
}

/// Pin the board the bundle was built for. It is often chosen by
/// `AVOCADO_TARGET_BOARD` or `--target-board`, which the hub does not have:
/// without this the hub composes the project for another board, and picks
/// up that board's extensions and run profiles. A runtime's own
/// `target_board` outranks `default_target_board`, so each one set is
/// rewritten too.
fn pin_board(doc: &mut serde_yaml::Value, board: &str) {
    let Some(map) = doc.as_mapping_mut() else {
        return;
    };
    map.insert("default_target_board".into(), board.into());
    for (_, rt) in map
        .get_mut("runtimes")
        .and_then(|r| r.as_mapping_mut())
        .into_iter()
        .flatten()
    {
        if let Some(rt) = rt
            .as_mapping_mut()
            .filter(|r| r.contains_key("target_board"))
        {
            rt.insert("target_board".into(), board.into());
        }
    }
}

/// A directory as a reproducible tar.gz: sorted, owner 0, mtime 0, the mode
/// kept; `.git` and `.avocado*` (build state) left out.
fn tar_dir(dir: &Path) -> Result<Vec<u8>> {
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut t = tar::Builder::new(gz);
    t.follow_symlinks(false);
    let walk = walkdir::WalkDir::new(dir)
        .min_depth(1)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            let n = e.file_name().to_string_lossy();
            n != ".git" && !n.starts_with(".avocado")
        });
    for e in walk {
        let e = e?;
        let rel = e.path().strip_prefix(dir)?;
        let md = e.path().symlink_metadata()?;
        let mut h = tar::Header::new_gnu();
        h.set_mtime(0);
        h.set_uid(0);
        h.set_gid(0);
        #[cfg(unix)]
        h.set_mode(std::os::unix::fs::PermissionsExt::mode(&md.permissions()) & 0o7777);
        #[cfg(not(unix))]
        h.set_mode(if md.is_dir() { 0o755 } else { 0o644 });
        if md.is_dir() {
            h.set_entry_type(tar::EntryType::Directory);
            h.set_size(0);
            t.append_data(&mut h, rel, std::io::empty())?;
        } else if md.file_type().is_symlink() {
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_size(0);
            t.append_link(&mut h, rel, std::fs::read_link(e.path())?)?;
        } else {
            h.set_entry_type(tar::EntryType::Regular);
            h.set_size(md.len());
            t.append_data(&mut h, rel, std::fs::File::open(e.path())?)?;
        }
    }
    Ok(t.into_inner()?.finish()?)
}

fn hex_sha256(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// An extension name usable as one directory name.
fn plain_name(n: &str) -> bool {
    !n.is_empty() && n != "." && n != ".." && !n.contains(['/', '\\', '\0'])
}

/// Unpack the bundle's path-sourced extensions where its avocado.yaml
/// expects them (`<cwd>/.avocado-bundle/ext/<name>`), before `sdk install`.
fn unpack_ext_sources(meta: &BundleMeta, cwd: &Path, cache: &Path) -> Result<()> {
    for a in &meta.ext_sources {
        if !plain_name(&a.name) || !is_sha256(&a.sha256) {
            bail!("bundle extension source {:?} is not valid", a.name);
        }
        let dir = cwd.join(WORK_DIR).join("ext").join(&a.name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let f = std::fs::File::open(cache.join(&a.sha256))?;
        tar::Archive::new(flate2::read::GzDecoder::new(f))
            .unpack(&dir)
            .with_context(|| format!("unpack extension '{}'", a.name))?;
    }
    Ok(())
}

/// Carry the feed the packing host resolved against (`AVOCADO_REPO_URL`, else
/// the config's) so the hub installs the same SDK and packages. Kept as
/// written: the bundle is portable, and a `--bundle` replay on this host
/// reaches its loopback as it is.
pub fn carry_feed(env: &mut HashMap<String, String>, repo_url: Option<String>) {
    if let Some(url) = repo_url {
        env.insert("AVOCADO_REPO_URL".into(), url);
    }
}

/// For a job on this machine's avocado-vm: a loopback feed becomes the
/// address the VM reaches this host at.
pub fn feed_for_local_vm(meta: &mut BundleMeta) {
    if let Some(url) = meta.env.get_mut("AVOCADO_REPO_URL") {
        *url = crate::utils::vm::hub::guest_api_url(url);
    }
}

/// The carried feed, for the hub's own `sdk install`.
fn carried_feed(meta: &BundleMeta) -> Option<(&str, &str)> {
    meta.env
        .get("AVOCADO_REPO_URL")
        .map(|u| ("AVOCADO_REPO_URL", u.as_str()))
}

/// A replay writes the bundle's avocado.yaml and avocado.lock into the
/// directory it runs in. Refuse one holding a project a replay didn't write
/// (no `.avocado-bundle/files.tsv`, which every replay writes before them);
/// a hub's work directory, reused job after job, passes.
fn check_replay_dir(cwd: &Path) -> Result<()> {
    if cwd.join(WORK_DIR).join("files.tsv").is_file() {
        return Ok(());
    }
    for f in ["avocado.yaml", "avocado.lock"] {
        if cwd.join(f).exists() {
            bail!(
                "{} was not written by a bundle replay, which would replace it; \
                 run `avocado provision --bundle` from an empty directory",
                cwd.join(f).display()
            );
        }
    }
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

/// The container script for `--bundle ... -- <argv>`: the command as given,
/// each word quoted, nothing restored or provisioned.
fn exec_script(argv: &[String]) -> String {
    let words: Vec<String> = argv
        .iter()
        .map(|a| crate::commands::rubicon::sh_quote(a))
        .collect();
    format!("exec {}\n", words.join(" "))
}

pub struct BundleProvision {
    /// A spec.json from a job, or a local bundle directory from `--pack`.
    pub source: String,
    pub profile: Option<String>,
    pub env_vars: Option<HashMap<String, String>>,
    pub verbose: bool,
    /// Run this in the bundle's SDK container instead of provisioning
    /// (empty: provision). Same SDK, env and container args as the
    /// provision, so a tool there (boardctl) sees the USB devices it would.
    pub exec: Vec<String>,
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
        check_replay_dir(&cwd)?;
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

        unpack_ext_sources(&meta, &cwd, &cache)?;
        // files.tsv first: it marks the directory as a replay's (see
        // `check_replay_dir`) before the project files are written.
        let tsv: String = meta
            .files
            .iter()
            .map(|f| format!("{}\t{}\t{}\n", f.sha256, f.mode, f.path))
            .collect();
        std::fs::write(work.join("files.tsv"), tsv)?;
        std::fs::write(cwd.join("avocado.yaml"), &meta.avocado_yaml)?;
        std::fs::write(cwd.join("avocado.lock"), &meta.avocado_lock)?;

        // The SDK matching the bundled lock. Reuses this directory's volume,
        // so later bundles for the same target skip the download.
        let exe = std::env::current_exe()?;
        let st = std::process::Command::new(exe)
            .args(["sdk", "install", "--no-tui", "--target", &meta.target])
            .envs(carried_feed(&meta))
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
        let script = if !self.exec.is_empty() {
            exec_script(&self.exec)
        } else {
            format!(
                r#"
cd "$AVOCADO_PREFIX" || exit 1
B=/opt/src/{WORK_DIR}
{clear}{small}while IFS=$'\t' read -r h mode path; do
    mkdir -p "$(dirname "$path")" && cp "$B/blobs/$h" "$path" && chmod "$mode" "$path" || exit 1
done < "$B/files.tsv" || exit 1
echo "[INFO] Restored the bundle into $AVOCADO_PREFIX"
{provision}"#,
                provision = meta.provision_script,
            )
        };
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
                // The restored includes are what the hook runs on: no live
                // extension sources mounted over them, read-write.
                ext_path_mounts: Some(HashMap::new()),
                ..Default::default()
            })
            .await
            .context("Failed to provision from bundle")?;
        if !self.exec.is_empty() {
            if !ok {
                bail!("{} failed in the bundle's SDK", self.exec[0]);
            }
            return Ok(());
        }
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

    /// Pack as `assemble` does: only what the target's runtimes need.
    fn pack(cfg: &Path, blobs: &Path) -> (Vec<ExtSourceArchive>, String) {
        let composed = Config::load_composed(cfg, Some("t")).unwrap();
        pack_ext_sources(
            cfg,
            "t",
            &required_extensions(&composed, "t").unwrap(),
            blobs,
        )
        .unwrap()
    }

    #[test]
    fn exec_runs_the_command_as_given_and_nothing_else() {
        let argv: Vec<String> = ["boardctl", "recovery", "it's; rm -rf /"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            exec_script(&argv),
            "exec 'boardctl' 'recovery' 'it'\\''s; rm -rf /'\n"
        );
    }

    #[test]
    fn the_packing_hosts_feed_rides_along_and_only_a_local_vm_job_rewrites_it() {
        let mut env = HashMap::new();
        carry_feed(&mut env, None);
        assert!(env.is_empty(), "no feed configured: nothing carried");
        // The bundle keeps the URL as written: a replay on this host reaches
        // its own loopback.
        carry_feed(&mut env, Some("http://localhost:8086".into()));
        assert_eq!(env["AVOCADO_REPO_URL"], "http://localhost:8086");

        let mut meta: BundleMeta = serde_json::from_value(serde_json::json!({
            "format": FORMAT, "target": "t", "runtime": "r", "sdk_host_arch": "x86_64",
            "env": env, "avocado_yaml": "", "avocado_lock": "", "provision_script": "",
            "clear": [], "files": [], "small_files": [], "created_at": "", "cli_version": ""
        }))
        .unwrap();
        assert_eq!(
            carried_feed(&meta),
            Some(("AVOCADO_REPO_URL", "http://localhost:8086"))
        );
        // Submitted to this machine's VM: the guest's way to the host.
        feed_for_local_vm(&mut meta);
        assert_eq!(meta.env["AVOCADO_REPO_URL"], "http://10.0.2.2:8086");
        meta.env
            .insert("AVOCADO_REPO_URL".into(), "http://10.10.0.10:8086".into());
        feed_for_local_vm(&mut meta);
        assert_eq!(meta.env["AVOCADO_REPO_URL"], "http://10.10.0.10:8086");
    }

    #[test]
    fn templated_path_sources_are_rewritten_too() {
        // The path itself is templated (on the board), not just the name: the
        // bundled config must still point at the carried copy.
        let tmp = tempfile::tempdir().unwrap();
        let ext = tmp.path().join("extensions/bsp-b1");
        std::fs::create_dir_all(&ext).unwrap();
        std::fs::write(ext.join("avocado.yaml"), "rubicon:\n  run: {}\n").unwrap();
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let cfg = project.join("avocado.yaml");
        std::fs::write(
            &cfg,
            "default_target: t\ndefault_target_board: b1\nextensions:\n  bsp-{{ avocado.target.board }}:\n    source:\n      type: path\n      path: ../extensions/bsp-{{ avocado.target.board }}\n",
        )
        .unwrap();
        let (a, yaml) = pack(&cfg, &tmp.path().join("blobs"));
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].name, "bsp-b1");
        assert!(yaml.contains(".avocado-bundle/ext/bsp-b1"), "{yaml}");
        assert!(!yaml.contains("../extensions"), "{yaml}");
    }

    #[test]
    fn only_the_targets_extensions_and_their_dependencies_are_carried() {
        // `bsp` is the runtime's and depends on `dep`; `other` belongs to no
        // runtime here and its checkout is absent: packing must not need it.
        let tmp = tempfile::tempdir().unwrap();
        for e in ["bsp", "dep"] {
            std::fs::create_dir_all(tmp.path().join(e)).unwrap();
            std::fs::write(tmp.path().join(e).join("avocado.yaml"), "{}\n").unwrap();
        }
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let cfg = project.join("avocado.yaml");
        std::fs::write(
            &cfg,
            "default_target: t\nruntimes:\n  r:\n    extensions: [bsp]\nextensions:\n  bsp:\n    source: { type: path, path: ../bsp }\n    depends_on: [dep]\n  dep:\n    source: { type: path, path: ../dep }\n  other:\n    source: { type: path, path: ../other }\n",
        )
        .unwrap();
        let (a, yaml) = pack(&cfg, &tmp.path().join("blobs"));
        let mut names: Vec<_> = a.iter().map(|x| x.name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["bsp", "dep"]);
        assert!(yaml.contains("../other"), "left as written: {yaml}");
    }

    #[test]
    fn path_sources_resolve_against_src_dir_and_the_replay_drops_it() {
        // The config sits in runtimes/b1 with `src_dir: ../..` (the repo
        // root): `path: bsp` is <root>/bsp, not runtimes/b1/bsp.
        let tmp = tempfile::tempdir().unwrap();
        let ext = tmp.path().join("bsp");
        std::fs::create_dir_all(&ext).unwrap();
        std::fs::write(ext.join("avocado.yaml"), "rubicon:\n  run: {}\n").unwrap();
        let project = tmp.path().join("runtimes/b1");
        std::fs::create_dir_all(&project).unwrap();
        let cfg = project.join("avocado.yaml");
        std::fs::write(
            &cfg,
            "src_dir: ../..\ndefault_target: t\nextensions:\n  bsp:\n    source:\n      type: path\n      path: bsp\n",
        )
        .unwrap();
        let (a, yaml) = pack(&cfg, &tmp.path().join("blobs"));
        assert_eq!(a.len(), 1);
        let doc: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        assert!(doc.get("src_dir").is_none(), "{yaml}");
        assert_eq!(
            doc["extensions"]["bsp"]["source"]["path"].as_str(),
            Some(".avocado-bundle/ext/bsp")
        );
    }

    #[test]
    fn path_sourced_image_sections_travel_inline() {
        // The rootfs comes from a fragment beside the project; the hub has
        // no such directory, so the bundled config carries the section.
        let tmp = tempfile::tempdir().unwrap();
        let frag = tmp.path().join("os/rootfs");
        std::fs::create_dir_all(&frag).unwrap();
        std::fs::write(
            frag.join("avocado.yaml"),
            "rootfs:\n  filesystem: erofs-lz4\n  packages:\n    pkg-rootfs: '*'\n",
        )
        .unwrap();
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let cfg = project.join("avocado.yaml");
        std::fs::write(
            &cfg,
            "default_target: t\nrootfs:\n  source:\n    type: path\n    path: ../os/rootfs\n",
        )
        .unwrap();
        let (_, yaml) = pack(&cfg, &tmp.path().join("blobs"));
        let doc: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        assert!(doc["rootfs"].get("source").is_none(), "{yaml}");
        assert_eq!(doc["rootfs"]["filesystem"].as_str(), Some("erofs-lz4"));

        // And the replay composes it with the fragment gone.
        std::fs::remove_dir_all(&frag).unwrap();
        let hub = tmp.path().join("hub");
        std::fs::create_dir_all(&hub).unwrap();
        std::fs::write(hub.join("avocado.yaml"), &yaml).unwrap();
        let composed = Config::load_composed(hub.join("avocado.yaml"), Some("t")).unwrap();
        assert_eq!(composed.config.get_rootfs_filesystem(), "erofs-lz4");
    }

    #[test]
    fn config_references_in_path_sources_are_rewritten() {
        let tmp = tempfile::tempdir().unwrap();
        let ext = tmp.path().join("extensions/bsp");
        std::fs::create_dir_all(&ext).unwrap();
        std::fs::write(ext.join("avocado.yaml"), "rubicon:\n  run: {}\n").unwrap();
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let cfg = project.join("avocado.yaml");
        std::fs::write(
            &cfg,
            "default_target: t\next_root: ../extensions\nextensions:\n  bsp:\n    source:\n      type: path\n      path: '{{ config.ext_root }}/bsp'\n",
        )
        .unwrap();
        let (a, yaml) = pack(&cfg, &tmp.path().join("blobs"));
        assert_eq!(a.len(), 1);
        assert!(yaml.contains(".avocado-bundle/ext/bsp"), "{yaml}");
        assert!(!yaml.contains("ext_root }}/bsp"), "{yaml}");
    }

    #[test]
    fn the_bundle_pins_the_board_it_was_built_for() {
        // The host resolved board b2 (here from the sole runtime's
        // target_board; on a bench, usually AVOCADO_TARGET_BOARD), not the
        // config's default b1. The hub has neither, so the bundled config
        // must name b2 itself.
        let tmp = tempfile::tempdir().unwrap();
        let ext = tmp.path().join("extensions/bsp-b2");
        std::fs::create_dir_all(&ext).unwrap();
        std::fs::write(ext.join("avocado.yaml"), "rubicon:\n  run: {}\n").unwrap();
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let cfg = project.join("avocado.yaml");
        std::fs::write(
            &cfg,
            "default_target: t\ndefault_target_board: b1\nruntimes:\n  r:\n    target_board: b2\n    extensions: ['bsp-{{ avocado.target.board }}']\nextensions:\n  bsp-{{ avocado.target.board }}:\n    source:\n      type: path\n      path: ../extensions/bsp-{{ avocado.target.board }}\n",
        )
        .unwrap();
        let (a, yaml) = pack(&cfg, &tmp.path().join("blobs"));
        assert_eq!(a[0].name, "bsp-b2");
        let doc: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(doc["default_target_board"].as_str(), Some("b2"), "{yaml}");
    }

    #[test]
    #[serial_test::serial] // reads AVOCADO_TARGET_BOARD, which serial tests set
    fn a_board_chosen_over_the_runtimes_own_is_pinned_over_it() {
        // Packed with AVOCADO_TARGET_BOARD=b2 while the runtime says b1: the
        // hub, without the env, must still compose b2.
        let mut doc: serde_yaml::Value = serde_yaml::from_str(
            "default_target_board: b0\nruntimes:\n  r:\n    target_board: b1\n",
        )
        .unwrap();
        pin_board(&mut doc, "b2");
        assert_eq!(doc["default_target_board"].as_str(), Some("b2"));
        let ctx =
            crate::utils::interpolation::AvocadoContext::from_main_config(&doc, Some("t"), None);
        assert_eq!(ctx.target_board.as_deref(), Some("b2"));
        // A runtime that names no board is left alone.
        let mut doc: serde_yaml::Value = serde_yaml::from_str("runtimes:\n  s: {}\n").unwrap();
        pin_board(&mut doc, "b2");
        assert!(doc["runtimes"]["s"].get("target_board").is_none());
    }

    #[test]
    fn path_extensions_travel_in_the_bundle_and_unpack_where_its_config_points() {
        let tmp = tempfile::tempdir().unwrap();
        let ext = tmp.path().join("bsp");
        std::fs::create_dir_all(ext.join("stone")).unwrap();
        std::fs::write(ext.join("avocado.yaml"), "rubicon:\n  run: {}\n").unwrap();
        std::fs::write(ext.join("stone/x.json"), "{}").unwrap();
        std::fs::create_dir_all(ext.join(".git")).unwrap();
        std::fs::write(ext.join(".git/HEAD"), "ref").unwrap();
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let cfg = project.join("avocado.yaml");
        std::fs::write(
            &cfg,
            "default_target: t\nextensions:\n  bsp-{{ avocado.target }}:\n    source:\n      type: path\n      path: ../bsp\n  pkg:\n    source:\n      type: package\n      version: '*'\n",
        )
        .unwrap();
        let blobs = tmp.path().join("blobs");
        let (a, yaml) = pack(&cfg, &blobs);
        assert_eq!(a.len(), 1, "only the path source is carried");
        assert_eq!(a[0].name, "bsp-t");
        // Deterministic: packing again gives the same blob.
        let (b, _) = pack(&cfg, &blobs);
        assert_eq!(a[0].sha256, b[0].sha256);
        assert!(yaml.contains(".avocado-bundle/ext/bsp-t"), "{yaml}");
        assert!(!yaml.contains("../bsp"), "{yaml}");

        let meta = BundleMeta {
            format: FORMAT,
            target: "t".into(),
            runtime: "r".into(),
            sdk_host_arch: "x86_64".into(),
            env: HashMap::new(),
            avocado_yaml: yaml,
            avocado_lock: String::new(),
            provision_script: String::new(),
            clear: vec![],
            files: vec![],
            small_files: vec![],
            created_at: String::new(),
            cli_version: String::new(),
            rubicon: None,
            ext_sources: a,
        };
        assert!(meta
            .blobs()
            .iter()
            .any(|x| x.sha256 == meta.ext_sources[0].sha256));
        let hub = tmp.path().join("hub");
        std::fs::create_dir_all(&hub).unwrap();
        unpack_ext_sources(&meta, &hub, &blobs).unwrap();
        let got = hub.join(".avocado-bundle/ext/bsp-t");
        assert!(got.join("avocado.yaml").is_file());
        assert!(got.join("stone/x.json").is_file());
        assert!(!got.join(".git").exists(), "repo metadata stays home");
    }

    #[test]
    fn a_replay_never_replaces_a_project_it_did_not_write() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        check_replay_dir(d).expect("an empty directory is fine");
        std::fs::write(d.join("avocado.lock"), "{}").unwrap();
        let e = check_replay_dir(d).unwrap_err().to_string();
        assert!(e.contains("avocado.lock"), "{e}");
        std::fs::write(d.join("avocado.yaml"), "default_target: t\n").unwrap();
        assert!(check_replay_dir(d).is_err(), "a project is refused");
        // A directory an earlier replay wrote (a hub's work dir) is reused.
        std::fs::create_dir_all(d.join(WORK_DIR)).unwrap();
        std::fs::write(d.join(WORK_DIR).join("files.tsv"), "").unwrap();
        check_replay_dir(d).expect("a replay's own files are replaced");
    }

    #[test]
    fn bundles_without_extension_sources_still_parse() {
        let old = r#"{"format":2,"target":"t","runtime":"r","sdk_host_arch":"x86_64","env":{},
            "avocado_yaml":"","avocado_lock":"","provision_script":"","clear":[],"files":[],
            "small_files":[],"created_at":"","cli_version":""}"#;
        let m: BundleMeta = serde_json::from_str(old).unwrap();
        assert!(m.ext_sources.is_empty());
        assert!(!serde_json::to_string(&m).unwrap().contains("ext_sources"));
    }

    #[test]
    fn bundle_rubicon_carries_only_referenced_run_profiles() {
        let config: Config = serde_yaml::from_str(
            r#"
rubicon:
  run:
    off: [ { out: { power: "off" } } ]
    enter: [ { run: off }, { out: { ssr1: "on" } } ]
    check: [ { wait_ms: 5 } ]
    unused: [ { wait_ms: 1 } ]
provision:
  tegraflash:
    rubicon: { before: enter, after: check }
  noop: {}
"#,
        )
        .unwrap();
        let r = bundle_rubicon(&config, Some("tegraflash".into()))
            .unwrap()
            .unwrap();
        assert_eq!(r.default_profile.as_deref(), Some("tegraflash"));
        assert_eq!(r.profiles.keys().collect::<Vec<_>>(), ["tegraflash"]);
        assert_eq!(r.run.keys().collect::<Vec<_>>(), ["check", "enter"]);
        // The hub reads it as JSON: steps are single-key objects.
        let json = serde_json::to_value(&r).unwrap();
        // `run:` is expanded for the hub.
        assert_eq!(
            json["run"]["enter"],
            serde_json::json!([{"out": {"power": "off"}}, {"out": {"ssr1": "on"}}])
        );

        let none: Config = serde_yaml::from_str("provision:\n  usb: {}\n").unwrap();
        assert!(bundle_rubicon(&none, None).unwrap().is_none());

        // No hooks, but the hub still needs the console rate and the wiring.
        let console: Config = serde_yaml::from_str(
            "rubicon:\n  console: { baud: 921600 }\n  wiring: { ssr1: REC }\nprovision:\n  usb: {}\n",
        )
        .unwrap();
        let r = bundle_rubicon(&console, None).unwrap().unwrap();
        assert!(r.profiles.is_empty() && r.run.is_empty());
        assert_eq!(r.console.and_then(|c| c.baud), Some(921600));
        assert_eq!(r.wiring["ssr1"], "REC");
    }

    #[test]
    fn stone_default_profile_reads_the_manifest() {
        let m = r#"{"runtime":{"provision_default":"tegraflash"},"provision":{"profiles":{}}}"#;
        assert_eq!(
            crate::commands::profiles::stone_default_profile(m).as_deref(),
            Some("tegraflash")
        );
        assert_eq!(crate::commands::profiles::stone_default_profile("{}"), None);
    }

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
            rubicon: None,
            ext_sources: vec![],
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

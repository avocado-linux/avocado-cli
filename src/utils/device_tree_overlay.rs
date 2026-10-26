//! Collection and validation of device-tree overlay declarations.
//!
//! An extension declares the device-tree overlays it ships under
//! `extensions.<ext>.device_tree_overlays`, each entry an object:
//!
//! ```yaml
//! ext:
//!   my-board:
//!     device_tree_overlays:
//!       - name: my-spi
//!         src: overlays/my-spi.dtso
//!         params:
//!           speed: "12000000"
//! ```
//!
//! An entry without `src` names a stock overlay the BSP already ships: it is
//! delivered and selected like any other, and nothing is compiled.
//!
//! An extension can also list `boot_config` lines for settings an overlay
//! source cannot express (a base `dtparam`, a firmware option). The BSP hook
//! decides how to apply them; see [`collect_boot_config`].
//!
//! ```yaml
//! ext:
//!   my-board:
//!     boot_config:
//!       - dtparam=i2c_arm=on
//!       - usb_max_current_enable=1
//! ```
//!
//! `name` is authoritative: it is the stone `out` basename
//! (`overlays/<name>.dtbo`), the RPi `config.txt` `dtoverlay=<name>` argument,
//! and the u-boot overlay entry. It must therefore be a safe basename. This is
//! deliberately NOT the pre-existing filesystem `overlay` key (see
//! `overlay_preprocess`), which copies files into an image; a device-tree
//! overlay is compiled and delivered to the boot medium.

use anyhow::{anyhow, bail, Result};
use serde_yaml::Value;
use std::collections::BTreeMap;

/// Root that a [`DeviceTreeOverlay::src`] is resolved against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SrcRoot {
    /// The project tree, bind-mounted at `/opt/src`: an extension defined
    /// inline, with no `source:`, keeps its overlays there.
    Project,
    /// The extension's own directory, `$AVOCADO_PREFIX/includes/<ext>`: any
    /// extension with a `source:`. A package source unpacks there; a path
    /// source is bind-mounted there.
    Extension,
}

/// One device-tree overlay declared by an extension enabled on a runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceTreeOverlay {
    /// Authoritative basename used for the .dtbo, the config.txt argument, and
    /// the u-boot entry.
    pub name: String,
    /// Source `.dtso`/`.dts`, relative to the root named by [`Self::src_root`].
    /// `None` names a stock overlay the BSP already ships: it is delivered and
    /// selected like any other, but there is nothing to compile.
    pub src: Option<String>,
    /// Where `src` is resolved from.
    ///
    /// An inline extension has its files in the project tree, bind-mounted
    /// at `/opt/src`. A PACKAGE-sourced extension does not: installing it
    /// unpacks only its `avocado.yaml` into `$AVOCADO_PREFIX/includes/<ext>/`,
    /// so a project-relative `src` names a file that is not in the container
    /// and the build fails with "source /opt/src/... does not exist" -- while
    /// the declaration reads as perfectly correct. Resolving against the
    /// extension's own include dir is what lets a BSP extension ship the
    /// overlay it declares.
    pub src_root: SrcRoot,
    /// Optional per-overlay parameters (e.g. `dtoverlay=name,key=value`).
    pub params: BTreeMap<String, String>,
    /// The extension that declared this overlay.
    pub ext_name: String,
}

/// Collect the device-tree overlays declared by every extension enabled on the
/// runtime, in declaration order. `merged_runtime` is the resolved runtime
/// value (its `extensions` sequence lists the enabled extensions); `parsed` is
/// the whole config (it holds `extensions.<name>.device_tree_overlays`).
///
/// Names must be unique across the whole runtime: two overlays sharing a name
/// would collide on the same `overlays/<name>.dtbo` output and boot-selection
/// argument, so a duplicate is a hard error rather than a silent
/// last-one-wins.
pub fn collect_for_runtime(
    merged_runtime: &Value,
    parsed: &Value,
) -> Result<Vec<DeviceTreeOverlay>> {
    let mut out: Vec<DeviceTreeOverlay> = Vec::new();
    let mut origin: BTreeMap<String, String> = BTreeMap::new();

    let Some(ext_list) = merged_runtime
        .get("extensions")
        .and_then(|e| e.as_sequence())
    else {
        return Ok(out);
    };

    for ext_val in ext_list {
        let Some(spec) =
            crate::utils::runtime_extension::RuntimeExtensionSpec::parse_entry(ext_val)
        else {
            continue;
        };
        let ext_name = spec.name.as_str();

        let Some(decls) = parsed
            .get("extensions")
            .and_then(|e| e.get(ext_name))
            .and_then(|e| e.get("device_tree_overlays"))
        else {
            continue;
        };

        let seq = decls.as_sequence().ok_or_else(|| {
            anyhow!("extension '{ext_name}': device_tree_overlays must be a list")
        })?;

        // A package-sourced extension's files are not in the project tree.
        // Installing one unpacks its avocado.yaml into includes/<ext>/, so the
        // overlay it declares has to be resolved from there.
        let src_root = match crate::utils::config::Config::parse_extension_source(
            ext_name,
            parsed
                .get("extensions")
                .and_then(|e| e.get(ext_name))
                .unwrap_or(&Value::Null),
        ) {
            Ok(Some(_)) => SrcRoot::Extension,
            _ => SrcRoot::Project,
        };

        for (idx, entry) in seq.iter().enumerate() {
            let overlay = parse_entry(entry, ext_name, idx, src_root.clone())?;
            if let Some(prev) = origin.insert(overlay.name.clone(), ext_name.to_string()) {
                bail!(
                    "device-tree overlay name '{}' is declared by both '{}' and '{}'; \
                     names must be unique across the runtime",
                    overlay.name,
                    prev,
                    ext_name
                );
            }
            out.push(overlay);
        }
    }

    Ok(out)
}

/// Collect the `boot_config` lines declared by every extension enabled on the
/// runtime, in declaration order, with repeated lines kept once.
///
/// Each entry is a single `key=value` line (for example
/// `dtparam=i2c_arm=on` or `usb_max_current_enable=1`) that the BSP's delivery
/// hook writes into the boot configuration. The CLI only checks the shape; what
/// a line means, and whether this BSP can apply it, is the hook's decision.
pub fn collect_boot_config(merged_runtime: &Value, parsed: &Value) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();

    let Some(ext_list) = merged_runtime
        .get("extensions")
        .and_then(|e| e.as_sequence())
    else {
        return Ok(out);
    };

    for ext_val in ext_list {
        let Some(spec) =
            crate::utils::runtime_extension::RuntimeExtensionSpec::parse_entry(ext_val)
        else {
            continue;
        };
        let ext_name = spec.name.as_str();

        let Some(decls) = parsed
            .get("extensions")
            .and_then(|e| e.get(ext_name))
            .and_then(|e| e.get("boot_config"))
        else {
            continue;
        };

        let seq = decls
            .as_sequence()
            .ok_or_else(|| anyhow!("extension '{ext_name}': boot_config must be a list"))?;

        for (idx, entry) in seq.iter().enumerate() {
            let line = entry.as_str().ok_or_else(|| {
                anyhow!("extension '{ext_name}': boot_config[{idx}] must be a string")
            })?;
            validate_boot_config_line(line, ext_name, idx)?;
            if !out.iter().any(|l| l == line) {
                out.push(line.to_string());
            }
        }
    }

    for (key, values) in conflicting_boot_config_keys(&out) {
        crate::utils::output::print_warning_above(&format!(
            "boot_config sets '{key}' to conflicting values ({}); the firmware uses the last \
             one, which follows the order of the runtime's extensions list",
            values.join(", ")
        ));
    }

    Ok(out)
}

/// Keys the firmware accepts more than once, so repeating one is not a conflict.
const REPEATABLE_BOOT_CONFIG_KEYS: &[&str] = &["dtparam", "dtoverlay", "gpio"];

/// Keys that appear with more than one distinct value, with the values in
/// first-seen order. Only the last value takes effect, so the outcome depends
/// on the order of the runtime's extensions list.
fn conflicting_boot_config_keys(lines: &[String]) -> Vec<(String, Vec<String>)> {
    let mut seen: Vec<(String, Vec<String>)> = Vec::new();
    for line in lines {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if REPEATABLE_BOOT_CONFIG_KEYS.contains(&key) {
            continue;
        }
        match seen.iter_mut().find(|(k, _)| k == key) {
            Some((_, values)) => {
                if !values.iter().any(|v| v == value) {
                    values.push(value.to_string());
                }
            }
            None => seen.push((key.to_string(), vec![value.to_string()])),
        }
    }
    seen.retain(|(_, values)| values.len() > 1);
    seen
}

/// A boot config line is written verbatim into a file the firmware parses
/// line by line, so a stray newline would inject a second setting and stray
/// whitespace would make the line not match what the BSP looks for.
fn validate_boot_config_line(line: &str, ext_name: &str, idx: usize) -> Result<()> {
    if line.chars().any(char::is_control) {
        bail!(
            "extension '{ext_name}': boot_config[{idx}] must be a single line with no \
             control characters"
        );
    }
    if line.trim().is_empty() {
        bail!("extension '{ext_name}': boot_config[{idx}] must not be empty");
    }
    if line != line.trim() {
        bail!("extension '{ext_name}': boot_config[{idx}] must not start or end with whitespace");
    }
    // The key is an identifier, which also rules out a `[gpio4=1]` conditional
    // filter and a `#x=y` comment: both contain '=' yet are not settings, and a
    // filter would silently scope every later line to one pin state.
    let key_ok = line.split_once('=').is_some_and(|(key, _)| {
        let mut chars = key.chars();
        chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    });
    if !key_ok {
        bail!("extension '{ext_name}': boot_config[{idx}] must be a key=value line, got '{line}'");
    }
    // A dtoverlay line here would bypass the uniqueness check, param rendering
    // and per-overlay claim that a device_tree_overlays entry gets.
    if line.starts_with("dtoverlay=") {
        bail!(
            "extension '{ext_name}': boot_config[{idx}] selects an overlay with '{line}'; \
             declare it under device_tree_overlays (omit src for an overlay the BSP ships)"
        );
    }
    Ok(())
}

/// Names of the active extensions that need the BSP delivery hook: those that
/// declare at least one device-tree overlay (compiled or stock) or any
/// `boot_config` line.
///
/// Unlike [`collect_for_runtime`], which scopes to a single runtime's enabled
/// extensions, this scans the whole active-extension set: the SDK install spans
/// every runtime, so the delivery hook must be provisioned whenever any active
/// extension declares one of them. Returns the declaring extension names
/// sorted; an empty result means the feature is inert.
pub fn active_extensions_declaring_overlays(
    parsed: &Value,
    active_extensions: &std::collections::HashSet<String>,
) -> Vec<String> {
    let non_empty = |ext: &str, key: &str| {
        parsed
            .get("extensions")
            .and_then(|e| e.get(ext))
            .and_then(|e| e.get(key))
            .and_then(|d| d.as_sequence())
            .is_some_and(|s| !s.is_empty())
    };
    let mut out: Vec<String> = active_extensions
        .iter()
        .filter(|ext| non_empty(ext, "device_tree_overlays") || non_empty(ext, "boot_config"))
        .cloned()
        .collect();
    out.sort();
    out
}

/// Names of the active extensions that declare at least one overlay with a
/// `src`, which is what needs the SDK build wrapper. A stock overlay or a
/// `boot_config` line compiles nothing, so those alone do not.
pub fn active_extensions_compiling_overlays(
    parsed: &Value,
    active_extensions: &std::collections::HashSet<String>,
) -> Vec<String> {
    let mut out: Vec<String> = active_extensions
        .iter()
        .filter(|ext| {
            parsed
                .get("extensions")
                .and_then(|e| e.get(ext.as_str()))
                .and_then(|e| e.get("device_tree_overlays"))
                .and_then(|d| d.as_sequence())
                .is_some_and(|s| s.iter().any(|o| o.get("src").is_some_and(|v| !v.is_null())))
        })
        .cloned()
        .collect();
    out.sort();
    out
}

fn parse_entry(
    entry: &Value,
    ext_name: &str,
    idx: usize,
    src_root: SrcRoot,
) -> Result<DeviceTreeOverlay> {
    let name = entry
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            anyhow!("extension '{ext_name}': device_tree_overlays[{idx}] missing 'name'")
        })?
        .to_string();
    validate_name(&name, ext_name)?;

    // An absent `src` names a stock overlay the BSP already ships, so there is
    // nothing to compile. A `src` that is present but empty or not a string is
    // a mistake, and treating it as a stock overlay would hide it.
    let src = match entry.get("src") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    anyhow!(
                        "extension '{ext_name}': device-tree overlay '{name}' has an unusable \
                         'src': it must be a non-empty string (omit it to name a stock overlay \
                         the BSP ships)"
                    )
                })?
                .to_string(),
        ),
    };

    let params = parse_params(entry.get("params"), ext_name, &name)?;

    Ok(DeviceTreeOverlay {
        name,
        src,
        src_root,
        params,
        ext_name: ext_name.to_string(),
    })
}

/// A name is used verbatim as a filename and a boot-loader argument, so it must
/// be a safe basename: no path separators, no whitespace, not a directory dot.
fn validate_name(name: &str, ext_name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("extension '{ext_name}': device-tree overlay name must not be empty");
    }
    if name == "." || name == ".." || name.contains('/') || name.contains(char::is_whitespace) {
        bail!(
            "extension '{ext_name}': device-tree overlay name '{name}' is not a valid basename \
             (no '/', whitespace, '.' or '..')"
        );
    }
    Ok(())
}

fn parse_params(
    value: Option<&Value>,
    ext_name: &str,
    name: &str,
) -> Result<BTreeMap<String, String>> {
    let mut params = BTreeMap::new();
    let Some(value) = value else {
        return Ok(params);
    };
    let mapping = value.as_mapping().ok_or_else(|| {
        anyhow!("extension '{ext_name}': device-tree overlay '{name}' params must be a mapping")
    })?;
    for (k, v) in mapping {
        let key = k.as_str().ok_or_else(|| {
            anyhow!(
                "extension '{ext_name}': device-tree overlay '{name}' has a non-string param key"
            )
        })?;
        let val = scalar_to_string(v).ok_or_else(|| {
            anyhow!(
                "extension '{ext_name}': device-tree overlay '{name}' param '{key}' must be a scalar"
            )
        })?;
        params.insert(key.to_string(), val);
    }
    Ok(params)
}

fn scalar_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Fixed sysroot-relative path of the single per-BSP delivery hook. Each BSP
/// layer installs its own implementation here; because only one BSP's layers
/// are present in a given machine build, exactly one is ever installed. The CLI
/// runs this exact path - it never scans a directory.
pub const DELIVERY_HOOK_RELATIVE: &str = "usr/libexec/avocado/device-tree-overlay-deliver";

/// nativesdk package providing the `avocado-dtc-overlay` SDK build wrapper.
/// Its RDEPENDS pulls `nativesdk-dtc`, and `stone` is a baseline SDK package
/// (stone bundle runs on every runtime build), so neither is provisioned
/// separately.
pub const SDK_OVERLAY_PACKAGE: &str = "nativesdk-avocado-dtc-overlay";

/// Target package providing the per-BSP `device-tree-overlay-deliver` hook
/// (the executable at [`DELIVERY_HOOK_RELATIVE`]). Installed into the
/// target-sysroot so the runtime build can invoke it.
pub const DELIVERY_HOOK_PACKAGE: &str = "avocado-dtc-overlay-deliver";

// In-container guard: fail the build if the hook staged overlays but left any
// unclaimed (declared and compiled, yet not delivered to the boot medium), or
// left boot_config lines unclaimed. The second check is what makes a hook that
// predates boot_config fail loudly instead of succeeding while ignoring them.
const CLAIMED_CHECK_PY: &str = r#"python3 - "$DTBO_STAGING/overlays.manifest.json" <<'AVOCADO_DTO_CHECK_EOF'
import json, sys
data = json.load(open(sys.argv[1]))
if data.get("version") != 1:
    sys.exit("[ERROR] overlays.manifest.json: unsupported version")
unclaimed = [o.get("name") for o in data.get("overlays", []) if not o.get("claimed_by")]
if unclaimed:
    sys.exit("[ERROR] device-tree overlays staged but not delivered by the BSP hook: " + ", ".join(unclaimed))
if data.get("boot_config") and not data.get("boot_config_claimed_by"):
    sys.exit("[ERROR] boot_config lines staged but not delivered by the BSP hook (this target's hook does not support boot_config): " + ", ".join(data["boot_config"]))
AVOCADO_DTO_CHECK_EOF
"#;

/// Build the CLI-authored `overlays.manifest.json`. `claimed_by` starts null;
/// the BSP hook sets it per overlay it delivers, and the in-container check
/// then fails on any that stayed null. `boot_config` follows the same shape
/// with one claim for the whole list. The manifest stays `version` 1: the new
/// fields are additive, and an older hook that ignores them leaves the claim
/// null, which the check turns into a build failure.
fn build_manifest_json(overlays: &[DeviceTreeOverlay], boot_config: &[String]) -> Result<String> {
    let arr: Vec<serde_json::Value> = overlays
        .iter()
        .map(|o| {
            serde_json::json!({
                "name": o.name,
                "file": format!("{}.dtbo", o.name),
                "params": o.params,
                "stock": o.src.is_none(),
                "claimed_by": serde_json::Value::Null,
            })
        })
        .collect();
    let doc = serde_json::json!({
        "version": 1,
        "overlays": arr,
        "boot_config": boot_config,
        "boot_config_claimed_by": serde_json::Value::Null,
    });
    Ok(serde_json::to_string_pretty(&doc)?)
}

/// Render the in-container shell block that builds, stages, delivers, and
/// validates the device-tree overlays, for injection into the runtime build
/// script immediately before `stone bundle`. With no overlays the block only
/// clears `$DTBO_STAGING`, so a previous build's overlays never carry into
/// this one; nothing is compiled or staged unless declared.
///
/// The block assumes `$STONE_MANIFEST` and `$STONE_INCLUDE_FLAGS` are already
/// set (they are by the time the build reaches the stone step) and leaves
/// `$STONE_INCLUDE_FLAGS` / `$STONE_OVERLAY_FLAG` updated for the bundle call.
pub fn render_build_section(
    overlays: &[DeviceTreeOverlay],
    boot_config: &[String],
) -> Result<String> {
    let mut s = String::new();

    // The staging directory describes THIS build, so it is cleared before every
    // one -- including the build that declares no overlays at all.
    //
    // The clear used to live below the empty-set early return, which meant it
    // only ran when there was something to stage. Removing the last overlay
    // (disabling an extension, or selecting a different board) then emitted no
    // script at all, the previous build's `apply-order` and .dtbo survived, and
    // the platform hook merged them into the next image exactly as designed:
    // its contract is "if apply-order exists, apply what it names", and it has
    // no way to tell a stale record from a current one.
    //
    // On rb3gen2 that shipped a mezzanine overlay into a core-kit build, whose
    // device tree enabled hardware that is not fitted. The build reported
    // success and the resulting UKI took the board down in firmware, through
    // both OTA and a fresh provision, because both carry the same image.
    s.push_str("# --- device-tree overlays ---\n");
    s.push_str(
        "DTBO_STAGING=\"$AVOCADO_PREFIX/output/runtimes/$RUNTIME_NAME/device-tree-overlays\"\n",
    );
    s.push_str("rm -rf \"$DTBO_STAGING\"\n");

    // boot_config alone still needs the hook: the settings reach the boot
    // medium only through it, and an early return here would drop them while
    // the build reported success.
    if overlays.is_empty() && boot_config.is_empty() {
        return Ok(s);
    }

    let manifest_json = build_manifest_json(overlays, boot_config)?;
    s.push_str("mkdir -p \"$DTBO_STAGING\"\n");
    s.push_str(&format!(
        "echo -e \"\\033[94m[INFO]\\033[0m Delivering {} device-tree overlay(s) and {} boot config line(s).\"\n",
        overlays.len(),
        boot_config.len()
    ));

    // name is a validated basename; src is user config, so double-quote it.
    // An overlay without a src is a stock overlay the BSP already ships: the
    // hook delivers and selects it, but there is nothing to compile.
    // The wrapper is installed by `avocado install` only when an overlay has a
    // source, and the SDK stamp does not track that, so a project that gained
    // its first source since the last install would otherwise fail with a bare
    // "command not found".
    if overlays.iter().any(|o| o.src.is_some()) {
        s.push_str("if ! command -v avocado-dtc-overlay >/dev/null 2>&1; then\n");
        s.push_str(
            "    echo \"[ERROR] avocado-dtc-overlay is not installed in the SDK; run 'avocado install' to add it.\" >&2\n",
        );
        s.push_str("    exit 1\n");
        s.push_str("fi\n");
    }
    for o in overlays {
        let Some(src) = &o.src else {
            continue;
        };
        // /opt/src is the project bind-mount; includes/<ext> is where a
        // package-sourced extension unpacks. Getting this wrong fails with
        // "source ... does not exist" on a declaration that is otherwise valid.
        let src_expr = match o.src_root {
            SrcRoot::Project => format!("/opt/src/{src}"),
            SrcRoot::Extension => {
                format!("$AVOCADO_PREFIX/includes/{}/{src}", o.ext_name)
            }
        };
        s.push_str(&format!(
            "avocado-dtc-overlay --name \"{}\" --src \"{}\" --out \"$DTBO_STAGING/{}.dtbo\"\n",
            o.name, src_expr, o.name
        ));
    }

    s.push_str("cat > \"$DTBO_STAGING/overlays.manifest.json\" <<'AVOCADO_DTO_MANIFEST_EOF'\n");
    s.push_str(&manifest_json);
    s.push_str("\nAVOCADO_DTO_MANIFEST_EOF\n");

    s.push_str(&format!(
        "DTO_HOOK=\"$OECORE_TARGET_SYSROOT/{DELIVERY_HOOK_RELATIVE}\"\n"
    ));
    s.push_str("DTO_FRAGMENT=\"$DTBO_STAGING/delivery.overlay.json\"\n");
    // D1: overlays declared but no hook installed for this BSP is a hard error,
    // not a silent skip - otherwise the build succeeds and ships nothing.
    s.push_str("if [ ! -x \"$DTO_HOOK\" ]; then\n");
    s.push_str(
        "    echo \"[ERROR] device-tree overlays or boot_config lines are declared but this target provides no delivery hook.\" >&2\n",
    );
    s.push_str(
        "    echo \"        Expected an executable at $DTO_HOOK (installed by the BSP layer).\" >&2\n",
    );
    s.push_str("    exit 1\n");
    s.push_str("fi\n");
    s.push_str("AVOCADO_DTBO_STAGING=\"$DTBO_STAGING\" \\\n");
    s.push_str("AVOCADO_OVERLAYS_MANIFEST=\"$DTBO_STAGING/overlays.manifest.json\" \\\n");
    s.push_str("AVOCADO_DELIVERY_FRAGMENT=\"$DTO_FRAGMENT\" \\\n");
    s.push_str("AVOCADO_STONE_MANIFEST=\"$STONE_MANIFEST\" \\\n");
    s.push_str("    \"$DTO_HOOK\"\n");

    s.push_str(CLAIMED_CHECK_PY);

    // D2 (2c): pass the hook-emitted fragment to stone via --overlay, and add
    // the staging dir to the -i include path so `in: <name>.dtbo` resolves.
    //
    // PREPENDED, not appended. stone resolves an image name against its -i dirs
    // and takes the first match (create.rs find_file_in_dirs), and the merged
    // DTB is published under the BSP's own base filename because the signer
    // regenerates kernel_<base>.dtb from it. Appending put the one directory
    // holding the merged tree behind all three others - the project's own
    // stone_include_paths, the runtime input dir, and the SDK stone dir - so a
    // file with that basename in any of them won the lookup and the board
    // flashed an unmerged tree: a clean boot with no overlay applied and
    // nothing reported. stone_include_paths is project-supplied, so that
    // collision is reachable from ordinary user config.
    //
    // Prepending makes the merged tree win by construction rather than by the
    // absence of a collision, which is the only version of this that a guard
    // cannot silently miss.
    s.push_str("STONE_INCLUDE_FLAGS=\"-i $DTBO_STAGING $STONE_INCLUDE_FLAGS\"\n");
    s.push_str("if [ -f \"$DTO_FRAGMENT\" ]; then\n");
    s.push_str("    STONE_OVERLAY_FLAG=\"--overlay $DTO_FRAGMENT\"\n");
    s.push_str("fi\n");
    s.push_str("# --- end device-tree overlays ---\n");

    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(yaml: &str) -> Value {
        serde_yaml::from_str(yaml).expect("valid test yaml")
    }

    // merged_runtime with the given enabled extension names.
    fn runtime_with(exts: &[&str]) -> Value {
        let list: String = exts.iter().map(|e| format!("  - {e}\n")).collect();
        cfg(&format!("extensions:\n{list}"))
    }

    #[test]
    fn collects_overlays_in_declaration_order_with_params() {
        let parsed = cfg(r#"
extensions:
  board-a:
    device_tree_overlays:
      - name: spi-fast
        src: overlays/spi-fast.dtso
        params:
          speed: "12000000"
          cs: 0
      - name: uart-rts
        src: overlays/uart-rts.dtso
  board-b:
    device_tree_overlays:
      - name: gpio-leds
        src: dts/leds.dtso
"#);
        let rt = runtime_with(&["board-a", "board-b"]);
        let got = collect_for_runtime(&rt, &parsed).unwrap();

        assert_eq!(
            got.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(),
            vec!["spi-fast", "uart-rts", "gpio-leds"]
        );
        assert_eq!(got[0].src.as_deref(), Some("overlays/spi-fast.dtso"));
        assert_eq!(got[0].ext_name, "board-a");
        assert_eq!(got[0].params.get("speed").unwrap(), "12000000");
        assert_eq!(got[0].params.get("cs").unwrap(), "0");
        assert!(got[1].params.is_empty());
        assert_eq!(got[2].ext_name, "board-b");
    }

    #[test]
    fn only_enabled_extensions_contribute() {
        let parsed = cfg(r#"
extensions:
  enabled:
    device_tree_overlays:
      - name: yes-overlay
        src: a.dtso
  disabled:
    device_tree_overlays:
      - name: no-overlay
        src: b.dtso
"#);
        let rt = runtime_with(&["enabled"]);
        let got = collect_for_runtime(&rt, &parsed).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "yes-overlay");
    }

    #[test]
    fn no_extensions_or_no_declarations_is_empty() {
        let parsed = cfg("extensions:\n  board:\n    packages:\n      - vim\n");
        assert!(collect_for_runtime(&cfg("{}"), &parsed).unwrap().is_empty());
        assert!(collect_for_runtime(&runtime_with(&["board"]), &parsed)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn duplicate_name_across_extensions_is_error() {
        let parsed = cfg(r#"
extensions:
  a:
    device_tree_overlays:
      - name: clash
        src: a.dtso
  b:
    device_tree_overlays:
      - name: clash
        src: b.dtso
"#);
        let err = collect_for_runtime(&runtime_with(&["a", "b"]), &parsed).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("clash"), "message names the overlay: {msg}");
        assert!(
            msg.contains('a') && msg.contains('b'),
            "names both exts: {msg}"
        );
    }

    #[test]
    fn missing_name_is_error() {
        let no_name = cfg("extensions:\n  a:\n    device_tree_overlays:\n      - src: a.dtso\n");
        assert!(collect_for_runtime(&runtime_with(&["a"]), &no_name)
            .unwrap_err()
            .to_string()
            .contains("missing 'name'"));
    }

    /// An absent `src` names a stock overlay the BSP ships; a `src` that is
    /// present but unusable is still a mistake, not a stock overlay.
    #[test]
    fn a_present_but_unusable_src_is_error() {
        for bad in ["\"\"", "\"   \"", "[a]", "{k: v}", "7"] {
            let parsed = cfg(&format!(
                "extensions:\n  a:\n    device_tree_overlays:\n      - name: x\n        src: {bad}\n"
            ));
            let err = collect_for_runtime(&runtime_with(&["a"]), &parsed)
                .expect_err(&format!("src {bad} must be rejected"))
                .to_string();
            assert!(err.contains("'src'"), "{bad}: {err}");
        }
    }

    #[test]
    fn an_overlay_without_src_is_a_stock_overlay_and_keeps_its_params() {
        let parsed = cfg(r#"
extensions:
  a:
    device_tree_overlays:
      - name: disable-bt
      - name: i2c1
        params:
          pins_2_3: true
      - name: mine
        src: overlays/mine.dtso
"#);
        let got = collect_for_runtime(&runtime_with(&["a"]), &parsed).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].name, "disable-bt");
        assert_eq!(got[0].src, None);
        assert!(got[0].params.is_empty());
        assert_eq!(got[1].src, None);
        assert_eq!(got[1].params.get("pins_2_3").unwrap(), "true");
        assert_eq!(got[2].src.as_deref(), Some("overlays/mine.dtso"));
    }

    #[test]
    fn unsafe_name_is_rejected() {
        for bad in ["../escape", "sub/dir", "has space", ".", ".."] {
            let parsed = cfg(&format!(
                "extensions:\n  a:\n    device_tree_overlays:\n      - name: \"{bad}\"\n        src: a.dtso\n"
            ));
            assert!(
                collect_for_runtime(&runtime_with(&["a"]), &parsed).is_err(),
                "name {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn declarations_must_be_a_list() {
        let parsed =
            cfg("extensions:\n  a:\n    device_tree_overlays:\n      name: x\n      src: a.dtso\n");
        assert!(collect_for_runtime(&runtime_with(&["a"]), &parsed)
            .unwrap_err()
            .to_string()
            .contains("must be a list"));
    }

    fn overlay(name: &str, src: &str) -> DeviceTreeOverlay {
        DeviceTreeOverlay {
            name: name.to_string(),
            src: Some(src.to_string()),
            src_root: SrcRoot::Project,
            params: BTreeMap::new(),
            ext_name: "board".to_string(),
        }
    }

    /// The empty set still clears the staging directory.
    ///
    /// This previously asserted the opposite -- that no overlays renders no
    /// script -- which is what let a stale `apply-order` from an earlier build
    /// survive into a build that declares nothing. The platform hook applies
    /// whatever `apply-order` names and cannot tell a stale record from a
    /// current one, so the next image silently carried the previous build's
    /// overlays.
    #[test]
    fn render_clears_staging_even_with_no_overlays() {
        let sh = render_build_section(&[], &[]).unwrap();

        assert!(
            sh.contains("rm -rf \"$DTBO_STAGING\""),
            "staging must be cleared when the overlay set is empty, or the \
             previous build's overlays are applied to this one: {sh}"
        );
        assert!(
            sh.contains("DTBO_STAGING=\"$AVOCADO_PREFIX/output/runtimes/$RUNTIME_NAME/device-tree-overlays\""),
            "the clear needs the path it clears: {sh}"
        );
        // Cleared, but nothing staged and no compiler invoked.
        assert!(!sh.contains("avocado-dtc-overlay"), "{sh}");
        assert!(!sh.contains("mkdir -p"), "{sh}");
    }

    /// A build that DOES declare overlays must still reset first, so an
    /// overlay dropped from the set does not linger alongside the current ones.
    #[test]
    fn render_clears_staging_before_staging_overlays() {
        let sh =
            render_build_section(&[overlay("spi-fast", "overlays/spi-fast.dtso")], &[]).unwrap();
        let clear = sh.find("rm -rf \"$DTBO_STAGING\"").expect("clear present");
        let stage = sh.find("avocado-dtc-overlay").expect("stage present");
        assert!(clear < stage, "the clear must precede any staging: {sh}");
    }

    #[test]
    fn a_compiled_overlay_guards_on_the_compile_wrapper_being_installed() {
        let sh = render_build_section(&[overlay("a", "a.dtso")], &[]).unwrap();
        let guard = sh
            .find("command -v avocado-dtc-overlay")
            .expect("a guard for the missing wrapper");
        let call = sh.find("avocado-dtc-overlay --name").expect("compile call");
        assert!(
            guard < call,
            "guard must run before the first compile: {sh}"
        );
        assert!(sh.contains("avocado install"), "{sh}");
        // Stock overlays and boot_config compile nothing, so need no wrapper.
        let sh = render_build_section(&[], &["dtparam=spi=on".to_string()]).unwrap();
        assert!(!sh.contains("command -v avocado-dtc-overlay"), "{sh}");
    }

    #[test]
    fn the_missing_hook_error_names_boot_config_too() {
        let sh = render_build_section(&[], &["dtparam=spi=on".to_string()]).unwrap();
        assert!(
            sh.contains("device-tree overlays or boot_config lines are declared"),
            "{sh}"
        );
    }

    #[test]
    fn render_emits_wrapper_hook_and_wiring() {
        let overlays = vec![
            overlay("spi-fast", "overlays/spi-fast.dtso"),
            overlay("uart-rts", "dts/uart.dtso"),
        ];
        let sh = render_build_section(&overlays, &[]).unwrap();

        assert!(sh.contains(
            "avocado-dtc-overlay --name \"spi-fast\" --src \"/opt/src/overlays/spi-fast.dtso\" --out \"$DTBO_STAGING/spi-fast.dtbo\""
        ));
        assert!(
            sh.contains("avocado-dtc-overlay --name \"uart-rts\" --src \"/opt/src/dts/uart.dtso\"")
        );
        // Single fixed-path hook, guarded, not a directory scan.
        assert!(sh.contains(DELIVERY_HOOK_RELATIVE));
        assert!(sh.contains("if [ ! -x \"$DTO_HOOK\" ]; then"));
        // Claim validation is present.
        assert!(sh.contains("staged but not delivered by the BSP hook"));
        // Stone wiring: fragment via --overlay and staging dir on the -i path.
        assert!(sh.contains("STONE_OVERLAY_FLAG=\"--overlay $DTO_FRAGMENT\""));
        assert!(sh.contains("-i $DTBO_STAGING"));
    }

    #[test]
    fn manifest_json_starts_unclaimed_with_params() {
        let mut params = BTreeMap::new();
        params.insert("speed".to_string(), "12000000".to_string());
        let overlays = vec![DeviceTreeOverlay {
            name: "spi".to_string(),
            src: Some("a.dtso".to_string()),
            src_root: SrcRoot::Project,
            params,
            ext_name: "board".to_string(),
        }];
        let json = build_manifest_json(&overlays, &[]).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["overlays"][0]["name"], "spi");
        assert_eq!(v["overlays"][0]["file"], "spi.dtbo");
        assert_eq!(v["overlays"][0]["params"]["speed"], "12000000");
        assert!(
            v["overlays"][0]["claimed_by"].is_null(),
            "claimed_by must start null so the hook must set it"
        );
    }

    #[test]
    fn declaring_overlays_scans_active_set_not_per_runtime() {
        let parsed = cfg(r#"
extensions:
  board-a:
    device_tree_overlays:
      - name: spi
        src: a.dtso
  board-b:
    packages:
      - vim
  board-c:
    device_tree_overlays:
      - name: gpio
        src: c.dtso
"#);
        let active: std::collections::HashSet<String> = ["board-a", "board-b"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // board-a declares and is active -> included; board-b active but declares
        // none -> excluded; board-c declares but is inactive -> excluded.
        assert_eq!(
            active_extensions_declaring_overlays(&parsed, &active),
            vec!["board-a".to_string()]
        );
    }

    #[test]
    fn declaring_overlays_empty_when_none_active_declare() {
        let parsed = cfg("extensions:\n  a:\n    packages:\n      - vim\n");
        let active: std::collections::HashSet<String> =
            ["a"].iter().map(|s| s.to_string()).collect();
        assert!(active_extensions_declaring_overlays(&parsed, &active).is_empty());
    }

    /// A package-sourced extension's overlay resolves from its own include
    /// dir, not the project tree.
    ///
    /// Regression: `src` was always rendered as `/opt/src/<src>`. Installing a
    /// package-sourced extension unpacks only its avocado.yaml into
    /// includes/<ext>/, so its `overlays/*.dtso` is not in the container at
    /// all, and the build failed with
    ///
    ///   avocado-dtc-overlay: <name>: source /opt/src/overlays/<f>.dtso does not exist
    ///
    /// on a declaration that is correct. A BSP extension shipping the overlay
    /// it declares is the whole point of the feature.
    #[test]
    fn a_package_sourced_extension_resolves_src_from_its_own_dir() {
        let cfg: Value = serde_yaml::from_str(
            r#"
extensions:
  local-ext:
    device_tree_overlays:
      - name: from-project
        src: overlays/from-project.dtso
  pkg-ext:
    source:
      type: package
      version: "*"
    device_tree_overlays:
      - name: from-package
        src: overlays/from-package.dtso
"#,
        )
        .unwrap();
        let runtime: Value = serde_yaml::from_str(
            "extensions:
  - local-ext
  - pkg-ext
",
        )
        .unwrap();

        let overlays = collect_for_runtime(&runtime, &cfg).expect("collects");
        assert_eq!(overlays.len(), 2, "both extensions declare one overlay");

        let local = overlays.iter().find(|o| o.name == "from-project").unwrap();
        let pkg = overlays.iter().find(|o| o.name == "from-package").unwrap();
        assert_eq!(local.src_root, SrcRoot::Project);
        assert_eq!(pkg.src_root, SrcRoot::Extension);

        let script = render_build_section(&overlays, &[]).expect("renders");
        assert!(
            script.contains("--src \"/opt/src/overlays/from-project.dtso\""),
            "a project-tree extension keeps the bind-mount path: {script}"
        );
        assert!(
            script
                .contains("--src \"$AVOCADO_PREFIX/includes/pkg-ext/overlays/from-package.dtso\""),
            "a package-sourced extension resolves from its include dir: {script}"
        );
        assert!(
            !script.contains("/opt/src/overlays/from-package.dtso"),
            "the package extension must NOT be looked up in the project tree: {script}"
        );
    }

    #[test]
    fn declaring_overlays_ignores_empty_declaration_list() {
        let parsed = cfg("extensions:\n  a:\n    device_tree_overlays: []\n");
        let active: std::collections::HashSet<String> =
            ["a"].iter().map(|s| s.to_string()).collect();
        assert!(active_extensions_declaring_overlays(&parsed, &active).is_empty());
    }

    fn active(names: &[&str]) -> std::collections::HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    // ---- boot_config ----

    #[test]
    fn boot_config_is_collected_in_declaration_order_and_deduplicated() {
        let parsed = cfg(r#"
extensions:
  board-a:
    boot_config:
      - dtparam=i2c_arm=on
      - usb_max_current_enable=1
  board-b:
    boot_config:
      - usb_max_current_enable=1
      - dtparam=spi=on
"#);
        let got = collect_boot_config(&runtime_with(&["board-a", "board-b"]), &parsed).unwrap();
        assert_eq!(
            got,
            vec![
                "dtparam=i2c_arm=on".to_string(),
                "usb_max_current_enable=1".to_string(),
                "dtparam=spi=on".to_string(),
            ]
        );
    }

    #[test]
    fn boot_config_of_a_disabled_extension_does_not_contribute() {
        let parsed = cfg(r#"
extensions:
  on:
    boot_config:
      - dtparam=i2c_arm=on
  off:
    boot_config:
      - dtparam=spi=on
"#);
        let got = collect_boot_config(&runtime_with(&["on"]), &parsed).unwrap();
        assert_eq!(got, vec!["dtparam=i2c_arm=on".to_string()]);
    }

    #[test]
    fn boot_config_absent_or_empty_collects_nothing() {
        let none = cfg("extensions:\n  a:\n    packages:\n      - vim\n");
        assert!(collect_boot_config(&runtime_with(&["a"]), &none)
            .unwrap()
            .is_empty());
        assert!(collect_boot_config(&cfg("{}"), &none).unwrap().is_empty());
        let empty = cfg("extensions:\n  a:\n    boot_config: []\n");
        assert!(collect_boot_config(&runtime_with(&["a"]), &empty)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_malformed_boot_config_line_is_rejected_by_name() {
        // Each is quoted so YAML hands the exact bytes to the validator.
        let bad = [
            "\"\"",
            "\"   \"",
            "\"dtparam\"",
            "\"[gpio4=1]\"",
            "\"#x=y\"",
            "\"=value\"",
            "\"9key=1\"",
            "\"bad key=1\"",
            "\"dtoverlay=disable-bt\"",
            "\" dtparam=spi=on\"",
            "\"dtparam=spi=on \"",
            "\"dtparam=spi=on\\ndtoverlay=x\"",
            "\"dtparam=spi=on\\tx\"",
            "\"dtparam=spi=on\\u0007\"",
        ];
        for line in bad {
            let parsed = cfg(&format!(
                "extensions:\n  board:\n    boot_config:\n      - {line}\n"
            ));
            let err = collect_boot_config(&runtime_with(&["board"]), &parsed)
                .expect_err(&format!("{line} must be rejected"))
                .to_string();
            assert!(
                err.contains("board") && err.contains("boot_config"),
                "{line}: the error names the extension and the key: {err}"
            );
        }
    }

    #[test]
    fn dtoverlay_in_boot_config_points_at_the_stock_overlay_entry() {
        let parsed = cfg("extensions:\n  board:\n    boot_config:\n      - dtoverlay=disable-bt\n");
        let err = collect_boot_config(&runtime_with(&["board"]), &parsed)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("device_tree_overlays"),
            "the error must name the supported way to select a stock overlay: {err}"
        );
    }

    #[test]
    fn conflicting_values_for_one_key_are_reported_but_repeatable_keys_are_not() {
        let lines: Vec<String> = [
            "usb_max_current_enable=1",
            "dtparam=i2c_arm=on",
            "usb_max_current_enable=0",
            "dtparam=spi=on",
            "gpio=4=op",
            "gpio=5=op",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let got = conflicting_boot_config_keys(&lines);
        assert_eq!(
            got,
            vec![(
                "usb_max_current_enable".to_string(),
                vec!["1".to_string(), "0".to_string()]
            )]
        );
        assert!(conflicting_boot_config_keys(&lines[..2]).is_empty());
    }

    #[test]
    fn boot_config_must_be_a_list_of_strings() {
        for body in [
            "    boot_config: dtparam=spi=on\n",
            "    boot_config:\n      - 7\n",
        ] {
            let parsed = cfg(&format!("extensions:\n  board:\n{body}"));
            let err = collect_boot_config(&runtime_with(&["board"]), &parsed)
                .unwrap_err()
                .to_string();
            assert!(err.contains("boot_config"), "{err}");
        }
    }

    // ---- stock overlays and the manifest ----

    fn stock(name: &str) -> DeviceTreeOverlay {
        DeviceTreeOverlay {
            name: name.to_string(),
            src: None,
            src_root: SrcRoot::Project,
            params: BTreeMap::new(),
            ext_name: "board".to_string(),
        }
    }

    #[test]
    fn manifest_marks_stock_overlays_and_carries_boot_config_unclaimed() {
        let overlays = vec![stock("disable-bt"), overlay("mine", "m.dtso")];
        let boot = vec!["dtparam=i2c_arm=on".to_string()];
        let v: serde_json::Value =
            serde_json::from_str(&build_manifest_json(&overlays, &boot).unwrap()).unwrap();

        assert_eq!(v["version"], 1);
        assert_eq!(v["overlays"][0]["name"], "disable-bt");
        assert_eq!(v["overlays"][0]["file"], "disable-bt.dtbo");
        assert_eq!(v["overlays"][0]["stock"], true);
        assert_eq!(v["overlays"][1]["stock"], false);
        assert_eq!(v["boot_config"], serde_json::json!(["dtparam=i2c_arm=on"]));
        assert!(
            v["boot_config_claimed_by"].is_null()
                && v.as_object()
                    .unwrap()
                    .contains_key("boot_config_claimed_by"),
            "the key exists and starts null so the hook has to set it: {v}"
        );
    }

    #[test]
    fn manifest_always_carries_the_boot_config_key_pair() {
        let v: serde_json::Value =
            serde_json::from_str(&build_manifest_json(&[overlay("a", "a.dtso")], &[]).unwrap())
                .unwrap();
        assert_eq!(v["boot_config"], serde_json::json!([]));
        assert!(v["boot_config_claimed_by"].is_null());
        assert!(v
            .as_object()
            .unwrap()
            .contains_key("boot_config_claimed_by"));
    }

    #[test]
    fn a_stock_overlay_is_delivered_but_never_compiled() {
        let sh = render_build_section(&[stock("disable-bt")], &[]).unwrap();
        assert!(
            !sh.contains("avocado-dtc-overlay --name"),
            "a stock overlay has no source to compile: {sh}"
        );
        assert!(sh.contains("\"stock\": true"), "{sh}");
        assert!(sh.contains(DELIVERY_HOOK_RELATIVE), "{sh}");
    }

    #[test]
    fn boot_config_alone_still_runs_the_hook_and_wires_stone() {
        let boot = vec![
            "dtparam=i2c_arm=on".to_string(),
            "usb_max_current_enable=1".to_string(),
        ];
        let sh = render_build_section(&[], &boot).unwrap();

        assert!(sh.contains("rm -rf \"$DTBO_STAGING\""), "{sh}");
        assert!(sh.contains("mkdir -p \"$DTBO_STAGING\""), "{sh}");
        assert!(!sh.contains("avocado-dtc-overlay --name"), "{sh}");
        assert!(sh.contains("if [ ! -x \"$DTO_HOOK\" ]; then"), "{sh}");
        assert!(sh.contains(DELIVERY_HOOK_RELATIVE), "{sh}");
        assert!(sh.contains("\"dtparam=i2c_arm=on\""), "{sh}");
        assert!(sh.contains("\"usb_max_current_enable=1\""), "{sh}");
        assert!(sh.contains("-i $DTBO_STAGING"), "{sh}");
        assert!(
            sh.contains("STONE_OVERLAY_FLAG=\"--overlay $DTO_FRAGMENT\""),
            "{sh}"
        );
    }

    /// Runs the in-container claim check against a manifest, as the build does.
    /// Returns None when no python3 is available on the host.
    fn run_claim_check(manifest: &serde_json::Value) -> Option<(bool, String)> {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("overlays.manifest.json"),
            serde_json::to_string(manifest).unwrap(),
        )
        .unwrap();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(CLAIMED_CHECK_PY)
            .env("DTBO_STAGING", dir.path())
            .output()
            .unwrap();
        Some((
            out.status.success(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))
    }

    #[test]
    fn an_unclaimed_boot_config_fails_the_claim_check() {
        let Some((ok, stderr)) = run_claim_check(&serde_json::json!({
            "version": 1,
            "overlays": [],
            "boot_config": ["dtparam=i2c_arm=on"],
            "boot_config_claimed_by": null,
        })) else {
            eprintln!("python3 not available, skipping");
            return;
        };
        assert!(
            !ok,
            "an older hook that ignores boot_config must fail the build"
        );
        assert!(stderr.contains("boot_config"), "{stderr}");
    }

    #[test]
    fn a_claimed_boot_config_and_empty_boot_config_pass_the_claim_check() {
        let claimed = serde_json::json!({
            "version": 1,
            "overlays": [],
            "boot_config": ["dtparam=i2c_arm=on"],
            "boot_config_claimed_by": "raspberrypi",
        });
        let none = serde_json::json!({
            "version": 1,
            "overlays": [],
            "boot_config": [],
            "boot_config_claimed_by": null,
        });
        let legacy = serde_json::json!({ "version": 1, "overlays": [] });
        for m in [claimed, none, legacy] {
            let Some((ok, stderr)) = run_claim_check(&m) else {
                eprintln!("python3 not available, skipping");
                return;
            };
            assert!(ok, "{m}: {stderr}");
        }
    }

    #[test]
    fn an_unclaimed_overlay_still_fails_the_claim_check() {
        let Some((ok, stderr)) = run_claim_check(&serde_json::json!({
            "version": 1,
            "overlays": [{"name": "x", "claimed_by": null}],
            "boot_config": [],
            "boot_config_claimed_by": null,
        })) else {
            eprintln!("python3 not available, skipping");
            return;
        };
        assert!(!ok);
        assert!(stderr.contains("x"), "{stderr}");
    }

    // ---- what the SDK install provisions ----

    #[test]
    fn the_hook_is_needed_for_boot_config_and_stock_overlays_too() {
        let parsed = cfg(r#"
extensions:
  only-boot:
    boot_config:
      - dtparam=i2c_arm=on
  only-stock:
    device_tree_overlays:
      - name: disable-bt
  nothing:
    packages:
      - vim
"#);
        assert_eq!(
            active_extensions_declaring_overlays(
                &parsed,
                &active(&["only-boot", "only-stock", "nothing"])
            ),
            vec!["only-boot".to_string(), "only-stock".to_string()]
        );
    }

    #[test]
    fn the_compile_wrapper_is_needed_only_for_overlays_with_a_source() {
        let parsed = cfg(r#"
extensions:
  only-boot:
    boot_config:
      - dtparam=i2c_arm=on
  only-stock:
    device_tree_overlays:
      - name: disable-bt
  compiles:
    device_tree_overlays:
      - name: mine
        src: m.dtso
  mixed:
    device_tree_overlays:
      - name: disable-bt2
      - name: mine2
        src: m2.dtso
"#);
        assert_eq!(
            active_extensions_compiling_overlays(
                &parsed,
                &active(&["only-boot", "only-stock", "compiles", "mixed"])
            ),
            vec!["compiles".to_string(), "mixed".to_string()]
        );
        assert!(active_extensions_compiling_overlays(&parsed, &active(&[])).is_empty());
    }
}

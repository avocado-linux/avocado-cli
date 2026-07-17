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
//! `name` is authoritative: it is the stone `out` basename
//! (`overlays/<name>.dtbo`), the RPi `config.txt` `dtoverlay=<name>` argument,
//! and the u-boot overlay entry. It must therefore be a safe basename. This is
//! deliberately NOT the pre-existing filesystem `overlay` key (see
//! `overlay_preprocess`), which copies files into an image; a device-tree
//! overlay is compiled and delivered to the boot medium.

use anyhow::{anyhow, bail, Result};
use serde_yaml::Value;
use std::collections::BTreeMap;

/// One device-tree overlay declared by an extension enabled on a runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceTreeOverlay {
    /// Authoritative basename used for the .dtbo, the config.txt argument, and
    /// the u-boot entry.
    pub name: String,
    /// Source `.dtso`/`.dts`, relative to the project root (bind-mounted at
    /// `/opt/src` in the SDK container).
    pub src: String,
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

        for (idx, entry) in seq.iter().enumerate() {
            let overlay = parse_entry(entry, ext_name, idx)?;
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

fn parse_entry(entry: &Value, ext_name: &str, idx: usize) -> Result<DeviceTreeOverlay> {
    let name = entry
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            anyhow!("extension '{ext_name}': device_tree_overlays[{idx}] missing 'name'")
        })?
        .to_string();
    validate_name(&name, ext_name)?;

    let src = entry
        .get("src")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "extension '{ext_name}': device-tree overlay '{name}' missing a non-empty 'src'"
            )
        })?
        .to_string();

    let params = parse_params(entry.get("params"), ext_name, &name)?;

    Ok(DeviceTreeOverlay {
        name,
        src,
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

// In-container guard: fail the build if the hook staged overlays but left any
// unclaimed (declared and compiled, yet not delivered to the boot medium).
const CLAIMED_CHECK_PY: &str = r#"python3 - "$DTBO_STAGING/overlays.manifest.json" <<'AVOCADO_DTO_CHECK_EOF'
import json, sys
data = json.load(open(sys.argv[1]))
if data.get("version") != 1:
    sys.exit("[ERROR] overlays.manifest.json: unsupported version")
unclaimed = [o.get("name") for o in data.get("overlays", []) if not o.get("claimed_by")]
if unclaimed:
    sys.exit("[ERROR] device-tree overlays staged but not delivered by the BSP hook: " + ", ".join(unclaimed))
AVOCADO_DTO_CHECK_EOF
"#;

/// Build the CLI-authored `overlays.manifest.json`. `claimed_by` starts null;
/// the BSP hook sets it per overlay it delivers, and the in-container check
/// then fails on any that stayed null.
fn build_manifest_json(overlays: &[DeviceTreeOverlay]) -> Result<String> {
    let arr: Vec<serde_json::Value> = overlays
        .iter()
        .map(|o| {
            serde_json::json!({
                "name": o.name,
                "file": format!("{}.dtbo", o.name),
                "params": o.params,
                "claimed_by": serde_json::Value::Null,
            })
        })
        .collect();
    let doc = serde_json::json!({ "version": 1, "overlays": arr });
    Ok(serde_json::to_string_pretty(&doc)?)
}

/// Render the in-container shell block that builds, stages, delivers, and
/// validates the device-tree overlays, for injection into the runtime build
/// script immediately before `stone bundle`. Empty string when there are no
/// overlays, so the feature is entirely inert unless declared.
///
/// The block assumes `$STONE_MANIFEST` and `$STONE_INCLUDE_FLAGS` are already
/// set (they are by the time the build reaches the stone step) and leaves
/// `$STONE_INCLUDE_FLAGS` / `$STONE_OVERLAY_FLAG` updated for the bundle call.
pub fn render_build_section(overlays: &[DeviceTreeOverlay]) -> Result<String> {
    if overlays.is_empty() {
        return Ok(String::new());
    }

    let manifest_json = build_manifest_json(overlays)?;
    let mut s = String::new();

    s.push_str("# --- device-tree overlays (ENG-2134) ---\n");
    s.push_str(
        "DTBO_STAGING=\"$AVOCADO_PREFIX/output/runtimes/$RUNTIME_NAME/device-tree-overlays\"\n",
    );
    s.push_str("rm -rf \"$DTBO_STAGING\"\n");
    s.push_str("mkdir -p \"$DTBO_STAGING\"\n");
    s.push_str(&format!(
        "echo -e \"\\033[94m[INFO]\\033[0m Building {} device-tree overlay(s).\"\n",
        overlays.len()
    ));

    // name is a validated basename; src is user config, so double-quote it.
    for o in overlays {
        s.push_str(&format!(
            "avocado-dtc-overlay --name \"{}\" --src \"/opt/src/{}\" --out \"$DTBO_STAGING/{}.dtbo\"\n",
            o.name, o.src, o.name
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
        "    echo \"[ERROR] device-tree overlays are declared but this target provides no delivery hook.\" >&2\n",
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
    s.push_str("STONE_INCLUDE_FLAGS=\"$STONE_INCLUDE_FLAGS -i $DTBO_STAGING\"\n");
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
        assert_eq!(got[0].src, "overlays/spi-fast.dtso");
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
    fn missing_name_or_src_is_error() {
        let no_name = cfg("extensions:\n  a:\n    device_tree_overlays:\n      - src: a.dtso\n");
        assert!(collect_for_runtime(&runtime_with(&["a"]), &no_name)
            .unwrap_err()
            .to_string()
            .contains("missing 'name'"));

        let no_src = cfg("extensions:\n  a:\n    device_tree_overlays:\n      - name: x\n");
        assert!(collect_for_runtime(&runtime_with(&["a"]), &no_src)
            .unwrap_err()
            .to_string()
            .contains("'src'"));
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
            src: src.to_string(),
            params: BTreeMap::new(),
            ext_name: "board".to_string(),
        }
    }

    #[test]
    fn render_is_empty_without_overlays() {
        assert_eq!(render_build_section(&[]).unwrap(), "");
    }

    #[test]
    fn render_emits_wrapper_hook_and_wiring() {
        let overlays = vec![
            overlay("spi-fast", "overlays/spi-fast.dtso"),
            overlay("uart-rts", "dts/uart.dtso"),
        ];
        let sh = render_build_section(&overlays).unwrap();

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
            src: "a.dtso".to_string(),
            params,
            ext_name: "board".to_string(),
        }];
        let json = build_manifest_json(&overlays).unwrap();
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
}

//! `avocado sbom --device`: reconcile the build SBOM against what a running
//! device reports as actually merged.
//!
//! A device has no rpmdb, so it can only report which images are merged
//! (over `avocadoctl`). This joins that report against the build's own
//! manifest (`ImageIds`) to decide which scopes describe what the device
//! has, and turns anything merged that no scope can account for into an
//! explicit "uncovered" entry instead of dropping it.
//!
//! Kept separate from `generate.rs`: everything here is a plain data shape
//! or a pure function over it, with no SPDX knowledge. Rewriting
//! `build_document`'s output stays in `generate.rs`.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeSet;

use crate::commands::sbom::generate::ImageIds;
use crate::utils::device::DeviceSpec;

/// `avocadoctl -o json runtime inspect`'s `RuntimeInfo`: the runtime's
/// declared name/version, as opposed to `DeviceRuntime::id`, the build id.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeviceRuntimeInfo {
    pub(crate) name: String,
}

/// One entry of `runtime inspect`'s `extensions[]`, carrying the full image
/// id. `ext status`'s own `imageId` is an 8-char prefix; don't join on it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeviceManifestExtension {
    pub(crate) name: String,
    pub(crate) version: String,
    #[serde(default)]
    pub(crate) image_id: Option<String>,
}

/// `avocadoctl -o json runtime inspect`'s top-level `Runtime`. No id means
/// the active runtime.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeviceRuntime {
    /// The build id (`manifest.json`'s top-level `id`).
    pub(crate) id: String,
    pub(crate) runtime: DeviceRuntimeInfo,
    #[serde(default)]
    pub(crate) extensions: Vec<DeviceManifestExtension>,
    #[serde(default)]
    pub(crate) os_build_id: Option<String>,
    #[serde(default)]
    pub(crate) initramfs_build_id: Option<String>,
}

/// One entry of `avocadoctl -o json ext status`. Its `imageId` (an 8-char
/// prefix) is not modeled here either, for the same reason.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeviceExtensionStatus {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) version: Option<String>,
    pub(crate) is_merged: bool,
    #[serde(default)]
    pub(crate) origin: Option<String>,
}

/// Both device queries, parsed.
#[derive(Debug)]
pub(crate) struct DeviceReport {
    pub(crate) runtime: DeviceRuntime,
    pub(crate) statuses: Vec<DeviceExtensionStatus>,
}

/// One `ext status` entry with `isMerged == true`, joined against the
/// runtime manifest's full image id where that join is meaningful.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MergedExtension {
    pub(crate) name: String,
    pub(crate) version: Option<String>,
    pub(crate) origin: Option<String>,
    /// `None` when there is nothing to join: an ad hoc `HITL` mount, a
    /// stale version, or something merged outside the manifest entirely.
    pub(crate) image_id: Option<String>,
}

/// One extension merged on the device that no scope in the build SBOM can
/// account for. Emitted as its own `software_Package` rather than dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UncoveredEntry {
    pub(crate) name: String,
    pub(crate) version: Option<String>,
    pub(crate) origin: Option<String>,
}

impl UncoveredEntry {
    /// `name [version] (origin: …)`, for the warning and the summary.
    pub(crate) fn label(&self) -> String {
        let version = self
            .version
            .as_deref()
            .map(|v| format!(" {v}"))
            .unwrap_or_default();
        let origin = self.origin.as_deref().unwrap_or("unknown");
        format!("{}{version} (origin: {origin})", self.name)
    }
}

/// The result of reconciling a device's merged set against the local
/// build's `ImageIds`.
pub(crate) struct KeptState {
    /// Scope names to keep in the device document. `rootfs`, `initramfs`,
    /// `includes` and `runtime:<rt>` are always members.
    pub(crate) kept_scopes: BTreeSet<String>,
    pub(crate) uncovered: Vec<UncoveredEntry>,
}

/// `ext status`'s merged entries, joined against `runtime inspect`'s
/// `extensions[]` by name and version (a version mismatch gives no id, not
/// a stale match). `origin == "HITL"` always forfeits the id.
pub(crate) fn joined_merged_extensions(
    runtime: &DeviceRuntime,
    statuses: &[DeviceExtensionStatus],
) -> Vec<MergedExtension> {
    statuses
        .iter()
        .filter(|s| s.is_merged)
        .map(|s| {
            let image_id = if s.origin.as_deref() == Some("HITL") {
                None
            } else {
                runtime
                    .extensions
                    .iter()
                    .find(|e| e.name == s.name && Some(e.version.as_str()) == s.version.as_deref())
                    .and_then(|e| e.image_id.clone())
            };
            MergedExtension {
                name: s.name.clone(),
                version: s.version.clone(),
                origin: s.origin.clone(),
                image_id,
            }
        })
        .collect()
}

/// Which scopes describe what the device actually has merged, and which
/// merged extensions no scope can account for. A merged extension is kept
/// only when its device-reported image id equals the local manifest's id
/// for `ext:<runtime>/<name>` and that scope was scanned. A nested remote
/// extension's id can match with its packages only under `includes`.
pub(crate) fn resolve_kept_and_uncovered(
    merged: &[MergedExtension],
    runtime: &str,
    local_images: &ImageIds,
    scanned_scopes: &BTreeSet<String>,
) -> KeptState {
    let mut kept_scopes: BTreeSet<String> = BTreeSet::new();
    kept_scopes.insert("rootfs".to_string());
    kept_scopes.insert("initramfs".to_string());
    kept_scopes.insert("includes".to_string());
    kept_scopes.insert(format!("runtime:{runtime}"));

    let mut uncovered = Vec::new();
    for entry in merged {
        let scope_name = format!("ext:{runtime}/{}", entry.name);
        let covered = scanned_scopes.contains(&scope_name)
            && match (&entry.image_id, local_images.get(&scope_name)) {
                (Some(device_id), Some(local)) => *device_id == local.image_id,
                _ => false,
            };
        if covered {
            kept_scopes.insert(scope_name);
        } else {
            uncovered.push(UncoveredEntry {
                name: entry.name.clone(),
                version: entry.version.clone(),
                origin: entry.origin.clone(),
            });
        }
    }

    KeptState {
        kept_scopes,
        uncovered,
    }
}

/// The device's active runtime build id disagreeing with the local
/// project's is routine (any rebuild or OTA moves it), so this warns rather
/// than fails. `None` when there's no local manifest to compare against.
pub(crate) fn runtime_id_warning(device_id: &str, local_build_id: Option<&str>) -> Option<String> {
    let local_id = local_build_id?;
    if local_id == device_id {
        return None;
    }
    Some(format!(
        "the device's active runtime build id ({device_id}) differs from this project's build \
         ({local_id}): one of them was rebuilt or updated since the other. Extensions are \
         still matched by image id; the rootfs, initramfs and runtime package lists below \
         are the local build's."
    ))
}

/// The device's reported `osBuildId`/`initramfsBuildId` against the local
/// build's own. `rootfs`/`initramfs` stay in the kept set regardless; a
/// mismatch only warns.
pub(crate) fn build_id_warnings(device: &DeviceRuntime, local_images: &ImageIds) -> Vec<String> {
    let mut warnings = Vec::new();
    for (scope, device_id, label) in [
        ("rootfs", device.os_build_id.as_deref(), "rootfs build id"),
        (
            "initramfs",
            device.initramfs_build_id.as_deref(),
            "initramfs build id",
        ),
    ] {
        let (Some(device_id), Some(local)) = (device_id, local_images.get(scope)) else {
            continue;
        };
        if device_id != local.image_id {
            warnings.push(format!(
                "the device's {label} ({device_id}) differs from this project's build \
                 ({}); the {scope} package list is the local build's, not necessarily what is \
                 running on the device.",
                local.image_id
            ));
        }
    }
    warnings
}

/// Marks the start of one query's output within the combined SSH session.
const RUNTIME_BEGIN: &str = "##AVOCADO-SBOM-DEVICE-RUNTIME-BEGIN##";
const RUNTIME_RC_PREFIX: &str = "##AVOCADO-SBOM-DEVICE-RUNTIME-RC:";
const EXT_BEGIN: &str = "##AVOCADO-SBOM-DEVICE-EXT-BEGIN##";
const EXT_RC_PREFIX: &str = "##AVOCADO-SBOM-DEVICE-EXT-RC:";

/// One SSH session running both `avocadoctl` queries, each wrapped in
/// markers carrying its own exit code, so a connection failure, an
/// unsupported `avocadoctl`, and unparseable output can each be told apart.
/// Same `ssh` flags as `runtime deploy`'s own script.
pub(crate) fn ssh_query_script(spec: &DeviceSpec) -> String {
    format!(
        r#"
set -u
SSH_DEST="{ssh_dest}"
SSH_PORT_ARGS="{ssh_port_args}"
ssh -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null \
    -o ConnectTimeout=10 \
    -o LogLevel=ERROR \
    $SSH_PORT_ARGS \
    "$SSH_DEST" '
set -u
echo "{runtime_begin}"
avocadoctl -o json runtime inspect
echo "{runtime_rc_prefix}$?##"
echo "{ext_begin}"
avocadoctl -o json ext status
echo "{ext_rc_prefix}$?##"
'
"#,
        ssh_dest = spec.ssh_destination(),
        ssh_port_args = spec.ssh_port_args(),
        runtime_begin = RUNTIME_BEGIN,
        runtime_rc_prefix = RUNTIME_RC_PREFIX,
        ext_begin = EXT_BEGIN,
        ext_rc_prefix = EXT_RC_PREFIX,
    )
}

/// Pulls the payload and exit code between a `begin` marker and its `rc`
/// marker out of `output`. `None` if `begin` never appears (a broken SSH
/// session never printed any markers).
fn extract_block(output: &str, begin: &str, rc_prefix: &str) -> Option<(String, i32)> {
    let after_begin = output.find(begin)?;
    let rest = &output[after_begin + begin.len()..];
    let rc_pos = rest.find(rc_prefix)?;
    let payload = rest[..rc_pos].trim_matches('\n').to_string();
    let after_rc = &rest[rc_pos + rc_prefix.len()..];
    let rc_str = after_rc.split("##").next()?;
    let rc = rc_str.trim().parse::<i32>().ok()?;
    Some((payload, rc))
}

/// Parses [`ssh_query_script`]'s combined output into both device queries.
/// A non-zero exit or unparseable output is a hard error naming the
/// specific `avocadoctl` command at fault.
pub(crate) fn parse_device_report(output: &str) -> Result<DeviceReport> {
    let (runtime_payload, runtime_rc) = extract_block(output, RUNTIME_BEGIN, RUNTIME_RC_PREFIX)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "the SSH session never ran `avocadoctl runtime inspect`; check that ssh can \
                 reach the device"
            )
        })?;
    anyhow::ensure!(
        runtime_rc == 0,
        "`avocadoctl -o json runtime inspect` exited {runtime_rc} on the device (an avocadoctl \
         older than `-o json` fails here): {}",
        runtime_payload.trim()
    );
    let runtime: DeviceRuntime =
        serde_json::from_str(runtime_payload.trim()).with_context(|| {
            format!(
                "could not parse `avocadoctl runtime inspect` output as JSON: {}",
                runtime_payload.trim()
            )
        })?;

    let (ext_payload, ext_rc) =
        extract_block(output, EXT_BEGIN, EXT_RC_PREFIX).ok_or_else(|| {
            anyhow::anyhow!("the SSH session ended before `avocadoctl ext status` ran")
        })?;
    anyhow::ensure!(
        ext_rc == 0,
        "`avocadoctl ext status` exited {ext_rc} on the device: {}",
        ext_payload.trim()
    );
    let statuses: Vec<DeviceExtensionStatus> = serde_json::from_str(ext_payload.trim())
        .with_context(|| {
            format!(
                "could not parse `avocadoctl ext status` output as JSON: {}",
                ext_payload.trim()
            )
        })?;

    Ok(DeviceReport { runtime, statuses })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::sbom::generate::ImageIds;

    fn images(entries: &[(&str, &str)]) -> ImageIds {
        let manifest = serde_json::json!({
            "extensions": entries
                .iter()
                .map(|(name, image_id)| serde_json::json!({
                    "name": name,
                    "version": "1.0",
                    "image_id": image_id,
                }))
                .collect::<Vec<_>>(),
        });
        ImageIds::from_manifest(&manifest, "dev")
    }

    fn scanned(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn an_unmerged_extension_is_dropped() {
        let runtime: DeviceRuntime = serde_json::from_str(
            r#"{"id":"build-1","manifestVersion":2,"builtAt":"now",
                "runtime":{"name":"dev","version":"1.0"},
                "extensions":[{"name":"app","version":"1.0","imageId":"full-app-id",
                                "imageType":null,"sha256":"abc"}],
                "active":true,"osBuildId":null,"initramfsBuildId":null}"#,
        )
        .unwrap();
        let statuses: Vec<DeviceExtensionStatus> = serde_json::from_str(
            r#"[{"name":"app","version":"1.0","isSysext":true,"isConfext":false,
                 "isMerged":false,"origin":"Dir","imageId":"abcd1234","imageType":null}]"#,
        )
        .unwrap();

        let merged = joined_merged_extensions(&runtime, &statuses);
        assert!(merged.is_empty(), "isMerged: false must not appear at all");
    }

    #[test]
    fn the_full_id_comes_from_inspect_never_from_the_8_char_status_id() {
        let runtime: DeviceRuntime = serde_json::from_str(
            r#"{"id":"build-1","manifestVersion":2,"builtAt":"now",
                "runtime":{"name":"dev","version":"1.0"},
                "extensions":[{"name":"app","version":"1.0",
                                "imageId":"11111111-2222-3333-4444-555555555555",
                                "imageType":null,"sha256":"abc"}],
                "active":true,"osBuildId":null,"initramfsBuildId":null}"#,
        )
        .unwrap();
        let statuses: Vec<DeviceExtensionStatus> = serde_json::from_str(
            r#"[{"name":"app","version":"1.0","isSysext":true,"isConfext":false,
                 "isMerged":true,"origin":"Dir","imageId":"11111111","imageType":null}]"#,
        )
        .unwrap();

        let merged = joined_merged_extensions(&runtime, &statuses);
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].image_id.as_deref(),
            Some("11111111-2222-3333-4444-555555555555")
        );
    }

    #[test]
    fn a_name_match_at_a_different_version_gives_no_id() {
        let runtime: DeviceRuntime = serde_json::from_str(
            r#"{"id":"build-1","manifestVersion":2,"builtAt":"now",
                "runtime":{"name":"dev","version":"1.0"},
                "extensions":[{"name":"app","version":"1.0","imageId":"full-id-v1",
                                "imageType":null,"sha256":"abc"}],
                "active":true,"osBuildId":null,"initramfsBuildId":null}"#,
        )
        .unwrap();
        let statuses: Vec<DeviceExtensionStatus> = serde_json::from_str(
            r#"[{"name":"app","version":"2.0","isSysext":true,"isConfext":false,
                 "isMerged":true,"origin":"Dir","imageId":null,"imageType":null}]"#,
        )
        .unwrap();

        let merged = joined_merged_extensions(&runtime, &statuses);
        assert_eq!(merged.len(), 1);
        assert!(merged[0].image_id.is_none());
    }

    #[test]
    fn hitl_is_always_uncovered_even_with_a_matching_name_and_version() {
        let runtime: DeviceRuntime = serde_json::from_str(
            r#"{"id":"build-1","manifestVersion":2,"builtAt":"now",
                "runtime":{"name":"dev","version":"1.0"},
                "extensions":[{"name":"app","version":"1.0","imageId":"full-id",
                                "imageType":null,"sha256":"abc"}],
                "active":true,"osBuildId":null,"initramfsBuildId":null}"#,
        )
        .unwrap();
        let statuses: Vec<DeviceExtensionStatus> = serde_json::from_str(
            r#"[{"name":"app","version":"1.0","isSysext":true,"isConfext":false,
                 "isMerged":true,"origin":"HITL","imageId":null,"imageType":null}]"#,
        )
        .unwrap();

        let merged = joined_merged_extensions(&runtime, &statuses);
        assert_eq!(merged.len(), 1);
        assert!(merged[0].image_id.is_none());
    }

    #[test]
    fn a_loose_raw_with_no_manifest_entry_gives_no_id() {
        let runtime: DeviceRuntime = serde_json::from_str(
            r#"{"id":"build-1","manifestVersion":2,"builtAt":"now",
                "runtime":{"name":"dev","version":"1.0"},
                "extensions":[],
                "active":true,"osBuildId":null,"initramfsBuildId":null}"#,
        )
        .unwrap();
        let statuses: Vec<DeviceExtensionStatus> = serde_json::from_str(
            r#"[{"name":"sideloaded","version":null,"isSysext":true,"isConfext":false,
                 "isMerged":true,"origin":"Loop:/root/sideloaded.raw","imageId":null,
                 "imageType":null}]"#,
        )
        .unwrap();

        let merged = joined_merged_extensions(&runtime, &statuses);
        assert_eq!(merged.len(), 1);
        assert!(merged[0].image_id.is_none());
    }

    #[test]
    fn kept_scopes_are_exactly_the_base_and_the_covered_extensions() {
        let merged = vec![
            MergedExtension {
                name: "app".to_string(),
                version: Some("1.0".to_string()),
                origin: Some("Dir".to_string()),
                image_id: Some("id-app".to_string()),
            },
            MergedExtension {
                name: "stale".to_string(),
                version: Some("0.9".to_string()),
                origin: Some("Dir".to_string()),
                image_id: None,
            },
        ];
        let local = images(&[("app", "id-app"), ("stale", "id-stale-current")]);

        let state = resolve_kept_and_uncovered(&merged, "dev", &local, &scanned(&["ext:dev/app"]));

        assert_eq!(
            state.kept_scopes,
            BTreeSet::from([
                "rootfs".to_string(),
                "initramfs".to_string(),
                "includes".to_string(),
                "runtime:dev".to_string(),
                "ext:dev/app".to_string(),
            ])
        );
        assert_eq!(state.uncovered.len(), 1);
        assert_eq!(state.uncovered[0].name, "stale");
    }

    #[test]
    fn an_extension_disabled_by_override_on_the_device_never_reaches_this_function() {
        let merged = vec![MergedExtension {
            name: "kept-app".to_string(),
            version: Some("1.0".to_string()),
            origin: Some("Dir".to_string()),
            image_id: Some("id-kept".to_string()),
        }];
        let local = images(&[("kept-app", "id-kept"), ("disabled-app", "id-disabled")]);

        let state =
            resolve_kept_and_uncovered(&merged, "dev", &local, &scanned(&["ext:dev/kept-app"]));

        assert!(state.kept_scopes.contains("ext:dev/kept-app"));
        assert!(!state.kept_scopes.contains("ext:dev/disabled-app"));
        assert!(state.uncovered.is_empty());
    }

    #[test]
    fn an_image_id_the_local_build_does_not_recognise_is_uncovered() {
        let merged = vec![MergedExtension {
            name: "app".to_string(),
            version: Some("1.0".to_string()),
            origin: Some("Dir".to_string()),
            image_id: Some("id-from-a-different-build".to_string()),
        }];
        let local = images(&[("app", "id-current-build")]);

        let state = resolve_kept_and_uncovered(&merged, "dev", &local, &scanned(&["ext:dev/app"]));

        assert!(!state.kept_scopes.contains("ext:dev/app"));
        assert_eq!(state.uncovered.len(), 1);
        assert_eq!(state.uncovered[0].name, "app");
    }

    #[test]
    fn an_extension_with_a_matching_id_but_no_scanned_scope_is_uncovered() {
        let merged = vec![MergedExtension {
            name: "app".to_string(),
            version: Some("1.0".to_string()),
            origin: Some("Dir".to_string()),
            image_id: Some("id-app".to_string()),
        }];
        let local = images(&[("app", "id-app")]);

        let state = resolve_kept_and_uncovered(&merged, "dev", &local, &scanned(&[]));

        assert!(!state.kept_scopes.contains("ext:dev/app"));
        assert_eq!(state.uncovered.len(), 1);
        assert_eq!(state.uncovered[0].name, "app");
    }

    #[test]
    fn the_shared_includes_root_is_always_kept() {
        let local = images(&[]);
        let state = resolve_kept_and_uncovered(&[], "dev", &local, &scanned(&["includes"]));

        assert!(state.kept_scopes.contains("includes"));
    }

    #[test]
    fn a_runtime_id_mismatch_warns() {
        assert!(runtime_id_warning("device-build", Some("local-build")).is_some());
        assert!(runtime_id_warning("same-build", Some("same-build")).is_none());
        assert!(runtime_id_warning("device-build", None).is_none());
    }

    #[test]
    fn a_rootfs_build_id_mismatch_warns() {
        let manifest = serde_json::json!({"os_bundle": {"os_build_id": "local-rootfs-id"}});
        let local = ImageIds::from_manifest(&manifest, "dev");

        let mismatched: DeviceRuntime = serde_json::from_str(
            r#"{"id":"b","manifestVersion":2,"builtAt":"now",
                "runtime":{"name":"dev","version":"1.0"},"extensions":[],
                "active":true,"osBuildId":"device-rootfs-id","initramfsBuildId":null}"#,
        )
        .unwrap();
        assert_eq!(build_id_warnings(&mismatched, &local).len(), 1);

        let matched: DeviceRuntime = serde_json::from_str(
            r#"{"id":"b","manifestVersion":2,"builtAt":"now",
                "runtime":{"name":"dev","version":"1.0"},"extensions":[],
                "active":true,"osBuildId":"local-rootfs-id","initramfsBuildId":null}"#,
        )
        .unwrap();
        assert!(build_id_warnings(&matched, &local).is_empty());

        // No local rootfs id: nothing to compare against, no warning.
        let no_local = ImageIds::default();
        assert!(build_id_warnings(&mismatched, &no_local).is_empty());
    }

    #[test]
    fn ssh_query_script_matches_deploys_own_ssh_options() {
        let spec = DeviceSpec::parse("admin@10.0.0.5:2222").unwrap();
        let script = ssh_query_script(&spec);
        assert!(script.contains("ssh -o StrictHostKeyChecking=no"));
        assert!(script.contains("-o UserKnownHostsFile=/dev/null"));
        assert!(script.contains("-o ConnectTimeout=10"));
        assert!(script.contains("-o LogLevel=ERROR"));
        assert!(script.contains("SSH_DEST=\"admin@10.0.0.5\""));
        assert!(script.contains("SSH_PORT_ARGS=\"-p 2222\""));
        assert!(script.contains("avocadoctl -o json runtime inspect"));
        assert!(script.contains("avocadoctl -o json ext status"));
    }

    #[test]
    fn parse_device_report_reads_both_queries_from_one_session() {
        let runtime_json = serde_json::json!({
            "id": "build-1", "manifestVersion": 2, "builtAt": "now",
            "runtime": {"name": "dev", "version": "1.0"},
            "extensions": [], "active": true,
            "osBuildId": null, "initramfsBuildId": null,
        })
        .to_string();
        let ext_json = serde_json::json!([{
            "name": "app", "version": "1.0", "isSysext": true, "isConfext": false,
            "isMerged": true, "origin": "Dir", "imageId": "abcd1234", "imageType": null,
        }])
        .to_string();
        let output = format!(
            "{RUNTIME_BEGIN}\n{runtime_json}\n{RUNTIME_RC_PREFIX}0##\n\
             {EXT_BEGIN}\n{ext_json}\n{EXT_RC_PREFIX}0##\n"
        );

        let report = parse_device_report(&output).unwrap();
        assert_eq!(report.runtime.id, "build-1");
        assert_eq!(report.runtime.runtime.name, "dev");
        assert_eq!(report.statuses.len(), 1);
        assert_eq!(report.statuses[0].name, "app");
    }

    #[test]
    fn parse_device_report_on_empty_output_is_an_error_not_an_empty_document() {
        assert!(parse_device_report("").is_err());
    }

    #[test]
    fn parse_device_report_on_garbage_output_is_an_error() {
        assert!(parse_device_report("connection reset by peer\n").is_err());
    }

    #[test]
    fn parse_device_report_surfaces_a_nonzero_avocadoctl_exit() {
        let output =
            format!("{RUNTIME_BEGIN}\nerror: unrecognized argument '-o'\n{RUNTIME_RC_PREFIX}2##\n");
        let err = parse_device_report(&output).unwrap_err();
        assert!(
            err.to_string().contains("runtime inspect") && err.to_string().contains('2'),
            "error should name the command and its exit code: {err}"
        );
    }

    #[test]
    fn parse_device_report_on_unparseable_json_is_an_error() {
        let output = format!(
            "{RUNTIME_BEGIN}\nnot json at all\n{RUNTIME_RC_PREFIX}0##\n\
             {EXT_BEGIN}\n[]\n{EXT_RC_PREFIX}0##\n"
        );
        assert!(parse_device_report(&output).is_err());
    }
}

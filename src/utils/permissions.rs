//! Shared shell-script generator for baking users and groups into an
//! image's `/etc/passwd`, `/etc/shadow`, and `/etc/group`.
//!
//! Used by:
//! - Extension builds (`ext build`) — legacy path, copies passwd/shadow/group
//!   from `$AVOCADO_PREFIX/rootfs/etc/` into the extension sysroot, then
//!   adds users/groups. Will be removed once the deprecation period ends.
//! - Rootfs / initramfs builds (`runtime build`) — new path, edits the
//!   files in the image's work directory in place (the base-passwd / shadow
//!   packages have already staged them there).
//!
//! The function consumes raw `serde_yaml::Mapping`s for users/groups so the
//! existing dynamic field handling (uid/gid/gecos/shell/home/groups/shadow
//! attributes) keeps working without re-typing every field.

use serde_yaml::Mapping;

/// Render the shell-script section that creates/updates users and groups
/// inside `etc_dir`.
///
/// * `users` — the `users:` mapping (username → attribute map), or `None`.
/// * `groups` — the `groups:` mapping (groupname → attribute map), or `None`.
/// * `etc_dir` — shell expression pointing at the target `/etc` directory.
///   Examples: `"$AVOCADO_EXT_SYSROOTS/myext/etc"`, `"$ROOTFS_WORK/etc"`.
///   Embedded verbatim into the script — the caller is responsible for
///   ensuring it resolves correctly at script-run time.
/// * `copy_from` — when `Some(dir)`, the script begins by copying
///   `passwd`, `shadow`, `group` from `dir` into `etc_dir`. When `None`,
///   the files are assumed to already exist at `etc_dir` (the package
///   install staged them).
///
/// Returns an empty string when both `users` and `groups` are `None`.
pub fn render_users_groups_script(
    users: Option<&Mapping>,
    groups: Option<&Mapping>,
    etc_dir: &str,
    copy_from: Option<&str>,
) -> String {
    if users.is_none() && groups.is_none() {
        return String::new();
    }

    let mut script_lines = Vec::new();
    let mut has_valid_users = false;
    script_lines.push("\n# Copy and manage user authentication files".to_string());

    // Optional copy of base passwd/shadow/group from a source dir
    // (e.g. the rootfs sysroot's /etc) into the target /etc.
    if let Some(src) = copy_from {
        script_lines.push(format!(
            r#"
# Copy authentication files into target /etc
echo "Copying /etc/passwd, /etc/shadow, and /etc/group from {src} to {etc_dir}"
mkdir -p "{etc_dir}"
cp "{src}/passwd" "{etc_dir}/passwd"
cp "{src}/shadow" "{etc_dir}/shadow"
cp "{src}/group" "{etc_dir}/group"
"#
        ));
    }

    // Auto-incrementing counters for uid/gid starting at 1000
    script_lines.push(
        "# Auto-incrementing counters for uid/gid\nCURRENT_UID=1000\nCURRENT_GID=1000\n"
            .to_string(),
    );

    // Process groups first (they might be referenced by users)
    if let Some(groups) = groups {
        script_lines.push("\n# Create groups".to_string());

        for (groupname_val, group_config) in groups {
            let groupname = match groupname_val.as_str() {
                Some(name) => name,
                None => continue,
            };

            if let Some(group_table) = group_config.as_mapping() {
                let gid = if let Some(gid_value) = group_table.get("gid") {
                    if let Some(gid_num) = gid_value.as_i64() {
                        gid_num.to_string()
                    } else if let Some(gid_num) = gid_value.as_u64() {
                        gid_num.to_string()
                    } else {
                        "$CURRENT_GID".to_string()
                    }
                } else {
                    "$CURRENT_GID".to_string()
                };

                let system_group = group_table
                    .get("system")
                    .and_then(|s| s.as_bool())
                    .unwrap_or(false);

                let password = group_table
                    .get("password")
                    .and_then(|p| p.as_str())
                    .unwrap_or("");

                let members = if let Some(members_value) = group_table.get("members") {
                    if let Some(members_array) = members_value.as_sequence() {
                        members_array
                            .iter()
                            .filter_map(|m| m.as_str())
                            .collect::<Vec<_>>()
                            .join(",")
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };

                let system_type = if system_group { " (system group)" } else { "" };
                let password_note = if !password.is_empty() {
                    " with password"
                } else {
                    ""
                };
                let members_msg = if !members.is_empty() {
                    format!(" and members: {members}")
                } else {
                    String::new()
                };
                let password_config = if !password.is_empty() {
                    format!("\n# Set group password for '{groupname}'\necho \"Note: Group password configured for '{groupname}'\"")
                } else {
                    String::new()
                };
                let g = awk_value(groupname);
                let m = awk_value(&members);
                let gid_check = if gid == "$CURRENT_GID" {
                    String::new()
                } else {
                    format!(
                        r#"
    EXISTING_GID=$(awk -F: -v g='{g}' '$1 == g {{ print $3 }}' "{etc_dir}/group")
    if [ "$EXISTING_GID" != "{gid}" ]; then
        echo "Group '{groupname}' exists with GID $EXISTING_GID but avocado.yaml sets {gid}. Set gid: $EXISTING_GID or drop it." >&2
        exit 1
    fi"#
                    )
                };

                script_lines.push(format!(
                    r#"
# Create group '{groupname}'{system_type}
echo "Creating group '{groupname}'"{password_note}
if ! awk -F: -v g='{g}' '$1 == g {{ f = 1 }} END {{ exit !f }}' "{etc_dir}/group"; then
    echo "{groupname}:x:{gid}:{members}" >> "{etc_dir}/group"
    echo "Group '{groupname}' created with GID {gid}{members_msg}"
    if [ "{gid}" = "$CURRENT_GID" ]; then
        CURRENT_GID=$((CURRENT_GID + 1))
    fi
else{gid_check}
    echo "Group '{groupname}' already exists, updating members"
    if [ -n "{members}" ]; then
        awk -F: -v OFS=: -v g='{g}' -v m='{m}' '$1 == g {{ $4 = m }} {{ print }}' "{etc_dir}/group" > "{etc_dir}/group.tmp" && mv "{etc_dir}/group.tmp" "{etc_dir}/group"
        echo "Updated members for group '{groupname}'"
    fi
fi{password_config}"#
                ));
            } else {
                // Simple group with just GID auto-assignment
                let g = awk_value(groupname);
                script_lines.push(format!(
                    r#"
# Create group '{groupname}'
echo "Creating group '{groupname}'"
if ! awk -F: -v g='{g}' '$1 == g {{ f = 1 }} END {{ exit !f }}' "{etc_dir}/group"; then
    echo "{groupname}:x:$CURRENT_GID:" >> "{etc_dir}/group"
    echo "Group '{groupname}' created with GID $CURRENT_GID"
    CURRENT_GID=$((CURRENT_GID + 1))
else
    echo "Group '{groupname}' already exists"
fi"#
                ));
            }
        }
    }

    // Process users
    if let Some(users) = users {
        let mut user_script_lines = Vec::new();

        for (username_val, user_config) in users {
            let username = match username_val.as_str() {
                Some(name) => name,
                None => continue,
            };

            if let Some(user_table) = user_config.as_mapping() {
                let password = user_table
                    .get("password")
                    .and_then(|p| p.as_str())
                    .unwrap_or("*");

                has_valid_users = true;

                let uid = if let Some(uid_value) = user_table.get("uid") {
                    if let Some(uid_num) = uid_value.as_i64() {
                        uid_num.to_string()
                    } else {
                        "$CURRENT_UID".to_string()
                    }
                } else {
                    "$CURRENT_UID".to_string()
                };

                let gecos = user_table
                    .get("gecos")
                    .and_then(|g| g.as_str())
                    .unwrap_or(username);

                let default_home = format!("/home/{username}");
                let home = user_table
                    .get("home")
                    .and_then(|h| h.as_str())
                    .unwrap_or(&default_home);

                let shell = user_table
                    .get("shell")
                    .and_then(|s| s.as_str())
                    .unwrap_or("/bin/sh");

                let groups_list = user_table
                    .get("groups")
                    .and_then(|g| g.as_sequence())
                    .map(|groups_array| {
                        groups_array
                            .iter()
                            .filter_map(|g| g.as_str())
                            .map(|s| s.to_string())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();

                // The primary GID: `gid` when set, else the GID of the first
                // entry in `groups`, else a user-private group with the UID.
                // The lookup runs only when the user is created, so an
                // existing user keeps its GID.
                let u = awk_value(username);
                let (gid, gid_lookup) = if let Some(gid_num) =
                    user_table.get("gid").and_then(|g| g.as_i64())
                {
                    (gid_num.to_string(), String::new())
                } else if let Some(primary) = groups_list.first() {
                    let p = awk_value(primary);
                    (
                        "$USER_GID".to_string(),
                        format!(
                            r#"
    USER_GID=$(awk -F: -v g='{p}' '$1 == g {{ print $3 }}' "{etc_dir}/group")
    if [ -z "$USER_GID" ]; then
        echo "User '{username}' has no gid and its first group '{primary}' is not in {etc_dir}/group. Add the group under groups: or set gid:." >&2
        exit 1
    fi"#
                        ),
                    )
                } else {
                    (
                        "$USER_GID".to_string(),
                        format!(
                            r#"
    USER_GID=$(awk -F: -v g='{u}' '$1 == g {{ print $3 }}' "{etc_dir}/group")
    if [ -z "$USER_GID" ]; then
        USER_GID={uid}
        echo "{username}:x:$USER_GID:" >> "{etc_dir}/group"
        echo "Group '{username}' created with GID $USER_GID"
    fi"#
                        ),
                    )
                };

                let last_change = user_table
                    .get("last_change")
                    .and_then(|l| l.as_i64())
                    .unwrap_or(19000);

                let min_days = user_table
                    .get("min_days")
                    .and_then(|m| m.as_i64())
                    .unwrap_or(0);

                let max_days = user_table
                    .get("max_days")
                    .and_then(|m| m.as_i64())
                    .unwrap_or(99999);

                let warn_days = user_table
                    .get("warn_days")
                    .and_then(|w| w.as_i64())
                    .unwrap_or(7);

                let inactive_days = user_table
                    .get("inactive_days")
                    .and_then(|i| i.as_i64())
                    .map(|i| i.to_string())
                    .unwrap_or_default();

                let expire_date = user_table
                    .get("expire_date")
                    .and_then(|e| e.as_i64())
                    .map(|e| e.to_string())
                    .unwrap_or_default();

                let disabled = user_table
                    .get("disabled")
                    .and_then(|d| d.as_bool())
                    .unwrap_or(false);

                let system_user = user_table
                    .get("system")
                    .and_then(|s| s.as_bool())
                    .unwrap_or(false);

                // The shadow entry is written with echo inside double quotes,
                // so the shell-special chars in a password hash are escaped.
                let escaped_password = password.replace("\\", "\\\\").replace("$", "\\$");

                let system_label = if system_user { " (system user)" } else { "" };
                let warning_message = if password.is_empty() {
                    format!("\necho \"[WARNING] User '{username}' will be able to login with NO PASSWORD\"")
                } else {
                    String::new()
                };
                let disabled_note = if disabled {
                    "\necho \"Note: User account is marked as disabled\""
                } else {
                    ""
                };

                // Create user in passwd file
                user_script_lines.push(format!(
                    r#"
# Create user '{username}'
echo "Creating user '{username}'{system_label}"{warning_message}
if ! awk -F: -v u='{u}' '$1 == u {{ f = 1 }} END {{ exit !f }}' "{etc_dir}/passwd"; then{gid_lookup}
    echo "{username}:x:{uid}:{gid}:{gecos}:{home}:{shell}" >> "{etc_dir}/passwd"
    echo "User '{username}' created with UID {uid}, GID {gid}, home '{home}', shell '{shell}'"

    if [ "{uid}" = "$CURRENT_UID" ]; then
        CURRENT_UID=$((CURRENT_UID + 1))
    fi
else
    echo "User '{username}' already exists, updating attributes"
fi"#
                ));

                // Create/update user in shadow file with comprehensive attributes
                user_script_lines.push(format!(
                    r#"
# Set password and shadow attributes for user '{username}'
echo "Setting password and aging policy for user '{username}'"
if awk -F: -v u='{u}' '$1 == u {{ f = 1 }} END {{ exit !f }}' "{etc_dir}/shadow"; then
    awk -F: -v u='{u}' '$1 != u' "{etc_dir}/shadow" > "{etc_dir}/shadow.tmp" && mv "{etc_dir}/shadow.tmp" "{etc_dir}/shadow"
    echo "{username}:{escaped_password}:{last_change}:{min_days}:{max_days}:{warn_days}:{inactive_days}:{expire_date}:" >> "{etc_dir}/shadow"
    echo "Updated shadow entry for existing user '{username}'"
else
    echo "{username}:{escaped_password}:{last_change}:{min_days}:{max_days}:{warn_days}:{inactive_days}:{expire_date}:" >> "{etc_dir}/shadow"
    echo "Added new user '{username}' to shadow file"
fi{disabled_note}"#
                ));

                // Add user to additional groups if specified
                if groups_list.len() > 1 {
                    user_script_lines.push(format!(
                        r#"
# Add user '{username}' to additional groups"#
                    ));

                    for group in &groups_list[1..] {
                        let g = awk_value(group);
                        user_script_lines.push(format!(
                            r#"
if awk -F: -v g='{g}' '$1 == g {{ f = 1 }} END {{ exit !f }}' "{etc_dir}/group"; then
    if ! awk -F: -v g='{g}' -v u='{u}' '$1 == g {{ n = split($4, m, ","); for (i = 1; i <= n; i++) if (m[i] == u) f = 1 }} END {{ exit !f }}' "{etc_dir}/group"; then
        awk -F: -v OFS=: -v g='{g}' -v u='{u}' '$1 == g {{ $4 = ($4 == "" ? u : $4 "," u) }} {{ print }}' "{etc_dir}/group" > "{etc_dir}/group.tmp" && mv "{etc_dir}/group.tmp" "{etc_dir}/group"
        echo "Added user '{username}' to group '{group}'"
    fi
else
    echo "Warning: Group '{group}' not found, cannot add user '{username}'"
fi"#
                        ));
                    }
                }
            }
        }

        if has_valid_users {
            script_lines.push("\n# Create and configure users".to_string());
            script_lines.extend(user_script_lines);
        }
    }

    // Set proper permissions only if we processed any users or groups
    if groups.is_some() || has_valid_users {
        script_lines.push(format!(
            r#"
# Set proper ownership and permissions for authentication files
chown root:root "{etc_dir}/passwd" "{etc_dir}/shadow" "{etc_dir}/group"
chmod 644 "{etc_dir}/passwd"
chmod 640 "{etc_dir}/shadow"
chmod 644 "{etc_dir}/group"
echo "Set proper permissions on authentication files""#
        ));
    }

    script_lines.join("")
}

/// Quote a value for `awk -v name='...'`. The shell reads the single quotes
/// and awk reads escape sequences in `-v` values, so both are escaped.
fn awk_value(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "'\\''")
}

/// Convert an `Option<&BTreeMap<String, serde_yaml::Value>>` (the shape
/// stored in [`crate::utils::config::PermissionsConfig`]) into an owned
/// `serde_yaml::Mapping` appropriate for [`render_users_groups_script`].
///
/// `BTreeMap` iterates in key order, so the resulting `Mapping` — and the
/// users/groups the script then provisions — are deterministic across builds.
///
/// Returns `None` if the input is `None` or empty.
pub fn mapping_from_map(
    src: Option<&std::collections::BTreeMap<String, serde_yaml::Value>>,
) -> Option<Mapping> {
    let map = src?;
    if map.is_empty() {
        return None;
    }
    let mut out = Mapping::new();
    for (k, v) in map {
        out.insert(serde_yaml::Value::String(k.clone()), v.clone());
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(password: &str) -> serde_yaml::Value {
        let mut m = Mapping::new();
        m.insert(
            serde_yaml::Value::String("password".to_string()),
            serde_yaml::Value::String(password.to_string()),
        );
        serde_yaml::Value::Mapping(m)
    }

    #[test]
    fn empty_inputs_produce_empty_script() {
        assert_eq!(render_users_groups_script(None, None, "/etc", None), "");
    }

    #[test]
    fn target_etc_is_substituted_verbatim() {
        let mut users = Mapping::new();
        users.insert(serde_yaml::Value::String("root".to_string()), user(""));
        let script = render_users_groups_script(Some(&users), None, "$ROOTFS_WORK/etc", None);
        assert!(script.contains("$ROOTFS_WORK/etc/passwd"));
        assert!(script.contains("$ROOTFS_WORK/etc/shadow"));
        assert!(script.contains("$ROOTFS_WORK/etc/group"));
        // No copy preamble when copy_from is None.
        assert!(!script.contains("cp \""));
    }

    #[test]
    fn copy_from_emits_preamble() {
        let mut users = Mapping::new();
        users.insert(serde_yaml::Value::String("root".to_string()), user(""));
        let script = render_users_groups_script(
            Some(&users),
            None,
            "$AVOCADO_EXT_SYSROOTS/myext/etc",
            Some("$AVOCADO_PREFIX/rootfs/etc"),
        );
        assert!(script.contains("cp \"$AVOCADO_PREFIX/rootfs/etc/passwd\""));
        assert!(script.contains("cp \"$AVOCADO_PREFIX/rootfs/etc/shadow\""));
        assert!(script.contains("cp \"$AVOCADO_PREFIX/rootfs/etc/group\""));
        assert!(script.contains("$AVOCADO_EXT_SYSROOTS/myext/etc"));
    }

    #[test]
    fn empty_password_emits_no_login_warning() {
        let mut users = Mapping::new();
        users.insert(serde_yaml::Value::String("root".to_string()), user(""));
        let script = render_users_groups_script(Some(&users), None, "/etc", None);
        assert!(script.contains("[WARNING] User 'root' will be able to login with NO PASSWORD"));
    }

    #[test]
    fn hashed_password_does_not_warn() {
        let mut users = Mapping::new();
        users.insert(
            serde_yaml::Value::String("alice".to_string()),
            user("$6$salt$hash"),
        );
        let script = render_users_groups_script(Some(&users), None, "/etc", None);
        assert!(!script.contains("[WARNING]"));
        assert!(script.contains("alice:\\$6\\$salt\\$hash"));
    }

    #[test]
    fn groups_only_still_runs_chown() {
        let mut groups = Mapping::new();
        let mut docker = Mapping::new();
        docker.insert(
            serde_yaml::Value::String("gid".to_string()),
            serde_yaml::Value::Number(999.into()),
        );
        groups.insert(
            serde_yaml::Value::String("docker".to_string()),
            serde_yaml::Value::Mapping(docker),
        );
        let script = render_users_groups_script(None, Some(&groups), "/etc", None);
        assert!(script.contains("Creating group 'docker'"));
        assert!(script.contains("chown root:root \"/etc/passwd\""));
    }

    #[test]
    fn mapping_from_map_provisions_in_sorted_order() {
        // ENG-2441: users/groups come from a BTreeMap, so mapping_from_map must
        // preserve key-sorted order — the generated passwd appends and the
        // auto-assigned UIDs follow it, and a stable order is what stops the
        // build id (and any UID a user omits) from churning every build.
        let mut users = std::collections::BTreeMap::new();
        users.insert("zeta".to_string(), user(""));
        users.insert("alpha".to_string(), user(""));
        users.insert("mike".to_string(), user(""));

        let mapping = mapping_from_map(Some(&users)).expect("non-empty map yields Some");
        let keys: Vec<&str> = mapping.iter().filter_map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["alpha", "mike", "zeta"]);

        let script = render_users_groups_script(Some(&mapping), None, "/etc", None);
        let a = script.find("Creating user 'alpha'").unwrap();
        let m = script.find("Creating user 'mike'").unwrap();
        let z = script.find("Creating user 'zeta'").unwrap();
        assert!(a < m && m < z, "users must be provisioned in sorted order");

        // Empty / absent maps still fold to nothing.
        assert!(mapping_from_map(None).is_none());
        assert!(mapping_from_map(Some(&std::collections::BTreeMap::new())).is_none());
    }

    #[cfg(unix)]
    fn mapping(yaml: &str) -> Mapping {
        serde_yaml::from_str(yaml).unwrap()
    }

    /// Seed `etc/{passwd,group,shadow}` under a temp dir and run the generated
    /// script against it under the same shell options the image builders use.
    /// `bash` rather than `sh`, since `sh` may be dash without `pipefail`.
    /// `chown` is stubbed since the test does not run as root.
    #[cfg(unix)]
    fn run_script(
        passwd: &str,
        group: &str,
        users: &str,
        groups: Option<&str>,
    ) -> (tempfile::TempDir, std::process::Output) {
        run_script_with_shadow(passwd, group, "", users, groups)
    }

    #[cfg(unix)]
    fn run_script_with_shadow(
        passwd: &str,
        group: &str,
        shadow: &str,
        users: &str,
        groups: Option<&str>,
    ) -> (tempfile::TempDir, std::process::Output) {
        let dir = tempfile::tempdir().unwrap();
        let etc = dir.path().join("etc");
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::write(etc.join("passwd"), passwd).unwrap();
        std::fs::write(etc.join("group"), group).unwrap();
        std::fs::write(etc.join("shadow"), shadow).unwrap();

        let users = mapping(users);
        let groups = groups.map(mapping);
        let script = render_users_groups_script(
            Some(&users),
            groups.as_ref(),
            &etc.display().to_string(),
            None,
        );
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(format!("set -euo pipefail; chown() {{ :; }}; {script}"))
            .output()
            .expect("bash ran");
        (dir, output)
    }

    #[cfg(unix)]
    fn read(dir: &tempfile::TempDir, file: &str) -> String {
        std::fs::read_to_string(dir.path().join("etc").join(file)).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn a_user_joins_an_empty_group_without_a_leading_comma() {
        let passwd = "root:x:0:0:root:/root:/bin/sh\n";
        let group = "root:x:0:\nusers:x:100:\ndialout:x:20:\n";
        let users = "app:\n  groups: [users, dialout]\n";
        let (dir, out) = run_script(passwd, group, users, None);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let got = read(&dir, "group");
        assert!(got.contains("\ndialout:x:20:app\n"), "{got}");
        assert!(!got.contains(":,"), "{got}");

        // A second run over the same files must not add the user twice.
        let (dir, out) = run_script(&read(&dir, "passwd"), &got, users, None);
        assert!(out.status.success());
        assert_eq!(read(&dir, "group"), got);
    }

    #[cfg(unix)]
    #[test]
    fn membership_matches_a_whole_member_not_a_substring() {
        let passwd = "root:x:0:0:root:/root:/bin/sh\n";
        let group = "root:x:0:\nusers:x:100:\napp-admin:x:1002:appuser\nvideo:x:44:app\n";
        let users = "app:\n  groups: [users, app-admin, video]\n";
        let (dir, out) = run_script(passwd, group, users, None);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let got = read(&dir, "group");
        assert!(got.contains("\napp-admin:x:1002:appuser,app\n"), "{got}");
        assert!(got.contains("\nvideo:x:44:app\n"), "{got}");
    }

    #[cfg(unix)]
    #[test]
    fn a_user_without_gid_takes_its_first_group_or_gets_a_private_group() {
        let passwd = "root:x:0:0:root:/root:/bin/sh\n";
        let group = "users:x:100:\n";

        // gid from groups[0].
        let (dir, out) = run_script(passwd, group, "app:\n  groups: [users]\n", None);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(read(&dir, "passwd").contains("\napp:x:1000:100:"));

        // groups[0] missing from the target: fail and say which.
        let (dir, out) = run_script(passwd, group, "app:\n  groups: [nosuch]\n", None);
        assert!(!out.status.success());
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("User 'app'") && err.contains("group 'nosuch'"),
            "{err}"
        );
        assert!(!read(&dir, "passwd").contains("app:"));

        // Neither gid nor groups: a user-private group with the UID.
        let (dir, out) = run_script(passwd, group, "app:\n  password: '*'\n", None);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(read(&dir, "passwd").contains("\napp:x:1000:1000:"));
        assert!(read(&dir, "group").contains("\napp:x:1000:\n"));

        // An existing user keeps its GID and gets no group.
        let (dir, out) = run_script(passwd, group, "root:\n  password: ''\n", None);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(read(&dir, "passwd"), passwd);
        assert_eq!(read(&dir, "group"), group);
    }

    #[cfg(unix)]
    #[test]
    fn a_group_that_exists_with_another_gid_fails_the_build() {
        let passwd = "root:x:0:0:root:/root:/bin/sh\n";
        let group = "root:x:0:\ndocker:x:998:\n";
        let (dir, out) = run_script(
            passwd,
            group,
            "root:\n  password: ''\n",
            Some("docker:\n  gid: 999\n"),
        );
        assert!(!out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(
                "Group 'docker' exists with GID 998 but avocado.yaml sets 999. Set gid: 998 or drop it.\n"
            ),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(read(&dir, "group"), group);

        // The same GID, or no gid at all, proceeds and updates members.
        let (dir, out) = run_script(
            passwd,
            group,
            "root:\n  password: ''\n",
            Some("docker:\n  gid: 998\n  members: [root]\n"),
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(read(&dir, "group").contains("\ndocker:x:998:root\n"));
        let (dir, out) = run_script(
            passwd,
            group,
            "root:\n  password: ''\n",
            Some("docker:\n  members: [root]\n"),
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(read(&dir, "group").contains("\ndocker:x:998:root\n"));
    }

    #[cfg(unix)]
    #[test]
    fn names_match_literally_not_as_patterns() {
        let passwd = "root:x:0:0:root:/root:/bin/sh\nsvc-app:x:1002:1002::/:/bin/sh\n";
        let group = "root:x:0:\nsvc.app:x:1001:\nsvc-app:x:1002:\nops:x:50:appXsvc\n";
        let shadow = "svc-app:*:1:0:99999:7:::\n";
        let users = "\
app.svc:\n  groups: [svc.app, ops]\n\
bob:\n  groups: [svc-app, svc.app]\n\
svc.app:\n  gid: 1001\n  password: 'h$h'\n";
        let groups = "svc.app:\n  gid: 1001\n";
        let (dir, out) = run_script_with_shadow(passwd, group, shadow, users, Some(groups));
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let got = read(&dir, "group");
        assert!(got.contains("\nsvc.app:x:1001:bob\n"), "{got}");
        assert!(got.contains("\nsvc-app:x:1002:\n"), "{got}");
        assert!(got.contains("\nops:x:50:appXsvc,app.svc\n"), "{got}");
        let got = read(&dir, "passwd");
        assert!(got.contains("\napp.svc:x:1000:1001:"), "{got}");
        assert!(got.contains("\nbob:x:1001:1002:"), "{got}");
        assert!(got.contains("\nsvc.app:x:1002:1001:"), "{got}");
        assert!(got.contains("\nsvc-app:x:1002:1002::/:/bin/sh\n"), "{got}");
        let got = read(&dir, "shadow");
        assert!(got.contains("svc-app:*:1:0:99999:7:::\n"), "{got}");
        assert!(got.contains("\nsvc.app:h$h:19000:"), "{got}");

        assert_eq!(awk_value(r"it's a\b"), r"it'\''s a\\b");
    }
}

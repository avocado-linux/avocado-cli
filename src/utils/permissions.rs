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

                script_lines.push(format!(
                    r#"
# Create group '{groupname}'{system_type}
echo "Creating group '{groupname}'"{password_note}
if ! grep -q "^{groupname}:" "{etc_dir}/group"; then
    echo "{groupname}:x:{gid}:{members}" >> "{etc_dir}/group"
    echo "Group '{groupname}' created with GID {gid}{members_msg}"
    if [ "{gid}" = "$CURRENT_GID" ]; then
        CURRENT_GID=$((CURRENT_GID + 1))
    fi
else
    echo "Group '{groupname}' already exists, updating members"
    if [ -n "{members}" ]; then
        sed -i "s|^{groupname}:x:{gid}:.*$|{groupname}:x:{gid}:{members}|" "{etc_dir}/group"
        echo "Updated members for group '{groupname}'"
    fi
fi{password_config}"#
                ));
            } else {
                // Simple group with just GID auto-assignment
                script_lines.push(format!(
                    r#"
# Create group '{groupname}'
echo "Creating group '{groupname}'"
if ! grep -q "^{groupname}:" "{etc_dir}/group"; then
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

                let gid = if let Some(gid_value) = user_table.get("gid") {
                    if let Some(gid_num) = gid_value.as_i64() {
                        gid_num.to_string()
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

                let groups_list = if let Some(groups_value) = user_table.get("groups") {
                    if let Some(groups_array) = groups_value.as_sequence() {
                        groups_array
                            .iter()
                            .filter_map(|g| g.as_str())
                            .map(|s| s.to_string())
                            .collect::<Vec<_>>()
                    } else {
                        vec![username.to_string()]
                    }
                } else {
                    vec![username.to_string()]
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

                // We use | as sed delimiter to avoid conflicts with / in
                // password hashes; we still need to escape the chars that
                // are special inside a sed replacement string itself.
                let escaped_password = password
                    .replace("\\", "\\\\")
                    .replace("&", "\\&")
                    .replace("$", "\\$");

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
if ! grep -q "^{username}:" "{etc_dir}/passwd"; then
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
if grep -q "^{username}:" "{etc_dir}/shadow"; then
    sed -i "s|^{username}:.*$|{username}:{escaped_password}:{last_change}:{min_days}:{max_days}:{warn_days}:{inactive_days}:{expire_date}:|" "{etc_dir}/shadow"
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
                        user_script_lines.push(format!(
                            r#"
if grep -q "^{group}:" "{etc_dir}/group"; then
    if ! grep "^{group}:" "{etc_dir}/group" | grep -q "{username}"; then
        sed -i "s|^{group}:\([^:]*\):\([^:]*\):\(.*\)$|{group}:\1:\2:\3,{username}|" "{etc_dir}/group"
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

/// Convert an `Option<&HashMap<String, serde_yaml::Value>>` (the shape
/// stored in [`crate::utils::config::PermissionsConfig`]) into an owned
/// `serde_yaml::Mapping` ref appropriate for [`render_users_groups_script`].
///
/// Returns `None` if the input is `None` or empty.
pub fn mapping_from_hashmap(
    src: Option<&std::collections::HashMap<String, serde_yaml::Value>>,
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
}

//! Surgical YAML editing for avocado.yaml that preserves comments and formatting.
//!
//! Uses line-level text operations rather than full YAML deserialization/serialization
//! so that user comments, blank lines, and formatting choices are retained.

use anyhow::{Context, Result};
use std::path::Path;

/// Which section of avocado.yaml a package belongs to.
#[derive(Debug, Clone)]
pub enum PackageScope {
    Extension(String),
    Runtime(String),
    Sdk,
}

/// Add one or more packages to the `packages:` block of the given scope in avocado.yaml.
///
/// Each package is inserted as `      <name>: "*"`.  If the package already exists
/// in that block (regardless of version), it is left unchanged.
///
/// Returns the list of packages that were actually added (skipping duplicates).
pub fn add_packages(
    config_path: &Path,
    scope: &PackageScope,
    packages: &[String],
) -> Result<Vec<String>> {
    let content = std::fs::read_to_string(config_path)
        .with_context(|| format!("Failed to read {}", config_path.display()))?;

    let (new_content, added) = add_packages_to_yaml(&content, scope, packages)?;

    if !added.is_empty() {
        std::fs::write(config_path, &new_content)
            .with_context(|| format!("Failed to write {}", config_path.display()))?;
    }

    Ok(added)
}

/// Remove one or more packages from the `packages:` block of the given scope.
///
/// Returns the list of packages that were actually removed.
pub fn remove_packages(
    config_path: &Path,
    scope: &PackageScope,
    packages: &[String],
) -> Result<Vec<String>> {
    let content = std::fs::read_to_string(config_path)
        .with_context(|| format!("Failed to read {}", config_path.display()))?;

    let (new_content, removed) = remove_packages_from_yaml(&content, scope, packages)?;

    if !removed.is_empty() {
        std::fs::write(config_path, &new_content)
            .with_context(|| format!("Failed to write {}", config_path.display()))?;
    }

    Ok(removed)
}

/// Pure-function core: operates on the YAML string, returns (new_content, added_packages).
fn add_packages_to_yaml(
    content: &str,
    scope: &PackageScope,
    packages: &[String],
) -> Result<(String, Vec<String>)> {
    let lines: Vec<&str> = content.lines().collect();

    // Find the `packages:` block inside the target scope section
    let (pkg_block_start, pkg_block_end, pkg_indent) = find_packages_block(&lines, scope)?;

    // Determine which packages already exist in the block
    let existing: std::collections::HashSet<String> = (pkg_block_start..pkg_block_end)
        .filter_map(|i| extract_package_name(lines[i]))
        .collect();

    let to_add: Vec<&String> = packages
        .iter()
        .filter(|p| !existing.contains(p.as_str()))
        .collect();

    if to_add.is_empty() {
        return Ok((content.to_string(), vec![]));
    }

    let mut result_lines: Vec<String> = lines[..pkg_block_end]
        .iter()
        .map(|l| l.to_string())
        .collect();

    let added: Vec<String> = to_add.iter().map(|p| p.to_string()).collect();
    for pkg in &to_add {
        result_lines.push(format!("{pkg_indent}{pkg}: \"*\""));
    }

    for line in &lines[pkg_block_end..] {
        result_lines.push(line.to_string());
    }

    let mut out = result_lines.join("\n");
    if content.ends_with('\n') && !out.ends_with('\n') {
        out.push('\n');
    }

    Ok((out, added))
}

/// Pure-function core: operates on the YAML string, returns (new_content, removed_packages).
fn remove_packages_from_yaml(
    content: &str,
    scope: &PackageScope,
    packages: &[String],
) -> Result<(String, Vec<String>)> {
    let lines: Vec<&str> = content.lines().collect();

    let (pkg_block_start, pkg_block_end, _) = find_packages_block(&lines, scope)?;

    let to_remove: std::collections::HashSet<&str> = packages.iter().map(|s| s.as_str()).collect();

    let mut removed = Vec::new();
    let mut result_lines: Vec<String> = Vec::with_capacity(lines.len());

    for (i, line) in lines.iter().enumerate() {
        if i >= pkg_block_start && i < pkg_block_end {
            if let Some(name) = extract_package_name(line) {
                if to_remove.contains(name.as_str()) {
                    removed.push(name);
                    continue; // skip this line
                }
            }
        }
        result_lines.push(line.to_string());
    }

    let mut out = result_lines.join("\n");
    if content.ends_with('\n') && !out.ends_with('\n') {
        out.push('\n');
    }

    Ok((out, removed))
}

/// Locate the `packages:` block within a scope section.
///
/// Returns `(block_start_line, block_end_line, entry_indent)` where
/// `block_start_line` is the first entry line after `packages:` and
/// `block_end_line` is the first line that is NOT an entry in that block.
fn find_packages_block(lines: &[&str], scope: &PackageScope) -> Result<(usize, usize, String)> {
    // Step 1: find the scope header line
    let scope_line = find_scope_start(lines, scope)?;

    // Step 2: find `packages:` within this scope (at a deeper indent)
    let scope_indent = leading_spaces(lines[scope_line]);

    let mut packages_line = None;
    for (i, line) in lines.iter().enumerate().skip(scope_line + 1) {
        if line.trim().is_empty() || line.trim().starts_with('#') {
            continue;
        }
        let indent = leading_spaces(line);
        if indent <= scope_indent {
            break;
        }
        let trimmed = line.trim();
        if trimmed == "packages:" {
            packages_line = Some(i);
            break;
        }
    }

    let packages_line = packages_line.ok_or_else(|| {
        anyhow::anyhow!(
            "No 'packages:' block found in the {} section",
            scope_label(scope)
        )
    })?;

    // Step 3: find the entries inside the packages block
    let pkg_key_indent = leading_spaces(lines[packages_line]);
    let entry_indent_min = pkg_key_indent + 1;

    let block_start = packages_line + 1;
    let mut block_end = block_start;

    for (i, line) in lines.iter().enumerate().skip(block_start) {
        if line.trim().is_empty() || line.trim().starts_with('#') {
            block_end = i + 1;
            continue;
        }
        let indent = leading_spaces(line);
        if indent < entry_indent_min {
            break;
        }
        block_end = i + 1;
    }

    // Determine the indent used for entries (peek at the first real entry)
    let entry_indent = (block_start..block_end)
        .find_map(|i| {
            let l = lines[i];
            if !l.trim().is_empty() && !l.trim().starts_with('#') {
                Some(" ".repeat(leading_spaces(l)))
            } else {
                None
            }
        })
        .unwrap_or_else(|| " ".repeat(pkg_key_indent + 2));

    Ok((block_start, block_end, entry_indent))
}

/// Find the starting line index of a scope section.
fn find_scope_start(lines: &[&str], scope: &PackageScope) -> Result<usize> {
    match scope {
        PackageScope::Extension(name) => {
            // Look for `extensions:` then `<name>:` nested inside
            let ext_section = find_top_level_key(lines, "extensions")?;
            let ext_indent = leading_spaces(lines[ext_section]);
            for (i, line) in lines.iter().enumerate().skip(ext_section + 1) {
                if line.trim().is_empty() || line.trim().starts_with('#') {
                    continue;
                }
                let indent = leading_spaces(line);
                if indent <= ext_indent {
                    break;
                }
                let trimmed = line.trim();
                if trimmed.starts_with(&format!("{name}:"))
                    || trimmed.starts_with(&format!("\"{name}\":"))
                    || trimmed.starts_with(&format!("'{name}':"))
                {
                    return Ok(i);
                }
            }
            anyhow::bail!("Extension '{name}' not found in avocado.yaml");
        }
        PackageScope::Runtime(name) => {
            let rt_section = find_top_level_key(lines, "runtimes")?;
            let rt_indent = leading_spaces(lines[rt_section]);
            for (i, line) in lines.iter().enumerate().skip(rt_section + 1) {
                if line.trim().is_empty() || line.trim().starts_with('#') {
                    continue;
                }
                let indent = leading_spaces(line);
                if indent <= rt_indent {
                    break;
                }
                let trimmed = line.trim();
                if trimmed.starts_with(&format!("{name}:"))
                    || trimmed.starts_with(&format!("\"{name}\":"))
                    || trimmed.starts_with(&format!("'{name}':"))
                {
                    return Ok(i);
                }
            }
            anyhow::bail!("Runtime '{name}' not found in avocado.yaml");
        }
        PackageScope::Sdk => find_top_level_key(lines, "sdk"),
    }
}

/// Find a top-level YAML key (zero-indent or first occurrence).
fn find_top_level_key(lines: &[&str], key: &str) -> Result<usize> {
    let pattern = format!("{key}:");
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with(&pattern) && leading_spaces(line) == 0 {
            return Ok(i);
        }
    }
    anyhow::bail!("Top-level key '{key}' not found in avocado.yaml");
}

/// Extract the package name from a YAML entry line like `      curl: "*"`
fn extract_package_name(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let colon_pos = trimmed.find(':')?;
    let name = trimmed[..colon_pos].trim();
    if name.is_empty() {
        return None;
    }
    // Strip YAML quoting if present
    let name = name
        .trim_start_matches('"')
        .trim_end_matches('"')
        .trim_start_matches('\'')
        .trim_end_matches('\'');
    Some(name.to_string())
}

fn leading_spaces(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn scope_label(scope: &PackageScope) -> String {
    match scope {
        PackageScope::Extension(name) => format!("extension '{name}'"),
        PackageScope::Runtime(name) => format!("runtime '{name}'"),
        PackageScope::Sdk => "SDK".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_CONFIG: &str = r#"default_target: "qemux86-64"

distro:
  channel: apollo-edge
  version: 0.1.0

runtimes:
  dev:
    extensions:
      - app
      - config
    packages:
      avocado-runtime: "0.1.0"

extensions:
  # Application extension
  app:
    types:
      - sysext
      - confext
    version: "0.2.0"

    # Install additional application dependencies
    packages:
      curl: "*"
      iperf3: "*"

  config:
    types:
      - confext
    version: "0.2.0"

sdk:
  image: "docker.io/avocadolinux/sdk:apollo-edge"

  packages:
    avocado-sdk-toolchain: "0.1.0"
"#;

    #[test]
    fn test_add_package_to_extension() {
        let (result, added) = add_packages_to_yaml(
            SAMPLE_CONFIG,
            &PackageScope::Extension("app".to_string()),
            &["wget".to_string()],
        )
        .unwrap();

        assert_eq!(added, vec!["wget"]);
        assert!(result.contains("wget: \"*\""));
        // Original packages still present
        assert!(result.contains("curl: \"*\""));
        assert!(result.contains("iperf3: \"*\""));
        // Comments preserved
        assert!(result.contains("# Application extension"));
        assert!(result.contains("# Install additional application dependencies"));
    }

    #[test]
    fn test_add_duplicate_package_is_noop() {
        let (result, added) = add_packages_to_yaml(
            SAMPLE_CONFIG,
            &PackageScope::Extension("app".to_string()),
            &["curl".to_string()],
        )
        .unwrap();

        assert!(added.is_empty());
        assert_eq!(result, SAMPLE_CONFIG);
    }

    #[test]
    fn test_add_package_to_runtime() {
        let (result, added) = add_packages_to_yaml(
            SAMPLE_CONFIG,
            &PackageScope::Runtime("dev".to_string()),
            &["kernel-tools".to_string()],
        )
        .unwrap();

        assert_eq!(added, vec!["kernel-tools"]);
        assert!(result.contains("kernel-tools: \"*\""));
        assert!(result.contains("avocado-runtime: \"0.1.0\""));
    }

    #[test]
    fn test_add_package_to_sdk() {
        let (result, added) =
            add_packages_to_yaml(SAMPLE_CONFIG, &PackageScope::Sdk, &["gcc".to_string()]).unwrap();

        assert_eq!(added, vec!["gcc"]);
        assert!(result.contains("gcc: \"*\""));
        assert!(result.contains("avocado-sdk-toolchain: \"0.1.0\""));
    }

    #[test]
    fn test_add_multiple_packages() {
        let (result, added) = add_packages_to_yaml(
            SAMPLE_CONFIG,
            &PackageScope::Extension("app".to_string()),
            &["wget".to_string(), "nginx".to_string(), "curl".to_string()],
        )
        .unwrap();

        // curl already exists so only wget and nginx added
        assert_eq!(added, vec!["wget", "nginx"]);
        assert!(result.contains("wget: \"*\""));
        assert!(result.contains("nginx: \"*\""));
    }

    #[test]
    fn test_remove_package_from_extension() {
        let (result, removed) = remove_packages_from_yaml(
            SAMPLE_CONFIG,
            &PackageScope::Extension("app".to_string()),
            &["iperf3".to_string()],
        )
        .unwrap();

        assert_eq!(removed, vec!["iperf3"]);
        assert!(!result.contains("iperf3"));
        assert!(result.contains("curl: \"*\""));
    }

    #[test]
    fn test_remove_nonexistent_package() {
        let (result, removed) = remove_packages_from_yaml(
            SAMPLE_CONFIG,
            &PackageScope::Extension("app".to_string()),
            &["nonexistent".to_string()],
        )
        .unwrap();

        assert!(removed.is_empty());
        assert_eq!(result, SAMPLE_CONFIG);
    }

    #[test]
    fn test_remove_package_from_runtime() {
        let (result, removed) = remove_packages_from_yaml(
            SAMPLE_CONFIG,
            &PackageScope::Runtime("dev".to_string()),
            &["avocado-runtime".to_string()],
        )
        .unwrap();

        assert_eq!(removed, vec!["avocado-runtime"]);
        assert!(!result.contains("avocado-runtime"));
    }

    #[test]
    fn test_remove_preserves_comments() {
        let (result, _) = remove_packages_from_yaml(
            SAMPLE_CONFIG,
            &PackageScope::Extension("app".to_string()),
            &["iperf3".to_string()],
        )
        .unwrap();

        assert!(result.contains("# Application extension"));
        assert!(result.contains("# Install additional application dependencies"));
    }

    #[test]
    fn test_extension_not_found() {
        let result = add_packages_to_yaml(
            SAMPLE_CONFIG,
            &PackageScope::Extension("nonexistent".to_string()),
            &["curl".to_string()],
        );

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Extension 'nonexistent' not found"));
    }

    #[test]
    fn test_trailing_newline_preserved() {
        let (result, _) = add_packages_to_yaml(
            SAMPLE_CONFIG,
            &PackageScope::Extension("app".to_string()),
            &["wget".to_string()],
        )
        .unwrap();

        assert!(result.ends_with('\n'));
    }

    #[test]
    fn test_roundtrip_add_then_remove() {
        let (after_add, added) = add_packages_to_yaml(
            SAMPLE_CONFIG,
            &PackageScope::Extension("app".to_string()),
            &["wget".to_string()],
        )
        .unwrap();
        assert_eq!(added, vec!["wget"]);

        let (after_remove, removed) = remove_packages_from_yaml(
            &after_add,
            &PackageScope::Extension("app".to_string()),
            &["wget".to_string()],
        )
        .unwrap();
        assert_eq!(removed, vec!["wget"]);

        // Should be back to original
        assert_eq!(after_remove, SAMPLE_CONFIG);
    }

    #[test]
    fn test_extract_package_name_variants() {
        assert_eq!(
            extract_package_name("      curl: \"*\""),
            Some("curl".to_string())
        );
        assert_eq!(
            extract_package_name("    avocado-runtime: \"0.1.0\""),
            Some("avocado-runtime".to_string())
        );
        assert_eq!(extract_package_name("  # a comment"), None);
        assert_eq!(extract_package_name(""), None);
        assert_eq!(extract_package_name("   "), None);
    }
}

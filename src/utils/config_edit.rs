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

/// Ensure avocado-ext-connect and avocado-ext-tunnels are present in avocado.yaml.
///
/// Adds them to the top-level `extensions:` section (as package sources) and to the
/// specified runtime's `extensions:` list. Returns `true` if any changes were made.
pub fn ensure_connect_extensions(config_path: &Path, runtime_name: &str) -> Result<bool> {
    let content = std::fs::read_to_string(config_path)
        .with_context(|| format!("Failed to read {}", config_path.display()))?;

    let (new_content, changed) = ensure_connect_extensions_in_yaml(&content, runtime_name)?;

    if changed {
        std::fs::write(config_path, &new_content)
            .with_context(|| format!("Failed to write {}", config_path.display()))?;
    }

    Ok(changed)
}

/// Pure-function core for ensure_connect_extensions.
fn ensure_connect_extensions_in_yaml(content: &str, runtime_name: &str) -> Result<(String, bool)> {
    let mut result = content.to_string();
    let mut changed = false;

    // Extensions in the order they should appear in the runtime's extensions list.
    // Earlier = higher precedence when merging overlays, so connect-config comes first.
    let connect_exts = [
        "avocado-ext-connect-config",
        "avocado-ext-connect",
        "avocado-ext-tunnels",
    ];

    // Step 1: Add extension definitions under `extensions:` if missing
    for ext_name in &connect_exts {
        if !has_extension_definition(&result, ext_name) {
            if *ext_name == "avocado-ext-connect-config" {
                result = add_confext_extension_definition(&result, ext_name)?;
            } else {
                result = add_extension_definition(&result, ext_name)?;
            }
            changed = true;
        }
    }

    // Step 2: Add to runtime's extensions list if missing.
    // avocado-ext-connect-config must appear before avocado-ext-connect.
    for ext_name in &connect_exts {
        if !has_runtime_extension_entry(&result, runtime_name, ext_name) {
            if *ext_name == "avocado-ext-connect-config" {
                // Insert before avocado-ext-connect if it exists, otherwise append
                result = add_runtime_extension_entry_before(
                    &result,
                    runtime_name,
                    ext_name,
                    "avocado-ext-connect",
                )?;
            } else {
                result = add_runtime_extension_entry(&result, runtime_name, ext_name)?;
            }
            changed = true;
        }
    }

    Ok((result, changed))
}

/// Check if an extension definition exists under the top-level `extensions:` key.
fn has_extension_definition(content: &str, ext_name: &str) -> bool {
    let lines: Vec<&str> = content.lines().collect();
    let ext_section = match find_top_level_key(&lines, "extensions") {
        Ok(idx) => idx,
        Err(_) => return false,
    };
    let ext_indent = leading_spaces(lines[ext_section]);

    for line in lines.iter().skip(ext_section + 1) {
        if line.trim().is_empty() || line.trim().starts_with('#') {
            continue;
        }
        let indent = leading_spaces(line);
        if indent <= ext_indent {
            break;
        }
        let trimmed = line.trim();
        if trimmed.starts_with(&format!("{ext_name}:"))
            || trimmed.starts_with(&format!("\"{ext_name}\":"))
        {
            return true;
        }
    }
    false
}

/// Add a package-source extension definition at the end of the `extensions:` section.
fn add_extension_definition(content: &str, ext_name: &str) -> Result<String> {
    let lines: Vec<&str> = content.lines().collect();
    let ext_section = find_top_level_key(&lines, "extensions")?;
    let ext_indent = leading_spaces(lines[ext_section]);

    // Find the end of the extensions section
    let mut ext_end = ext_section + 1;
    for (i, line) in lines.iter().enumerate().skip(ext_section + 1) {
        if line.trim().is_empty() || line.trim().starts_with('#') {
            ext_end = i + 1;
            continue;
        }
        let indent = leading_spaces(line);
        if indent <= ext_indent {
            break;
        }
        ext_end = i + 1;
    }

    let entry_indent = " ".repeat(ext_indent + 2);
    let sub_indent = " ".repeat(ext_indent + 4);

    let mut result_lines: Vec<String> = lines[..ext_end].iter().map(|l| l.to_string()).collect();

    result_lines.push(String::new());
    result_lines.push(format!("{entry_indent}{ext_name}:"));
    result_lines.push(format!("{sub_indent}source:"));
    result_lines.push(format!("{sub_indent}  type: package"));
    result_lines.push(format!("{sub_indent}  version: \"*\""));

    // Ensure a blank line separates the new extension from the next section
    if let Some(next_line) = lines.get(ext_end) {
        if !next_line.trim().is_empty() {
            result_lines.push(String::new());
        }
    }

    for line in &lines[ext_end..] {
        result_lines.push(line.to_string());
    }

    let mut out = result_lines.join("\n");
    if content.ends_with('\n') && !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Add a confext extension definition (types: confext, overlay dir) at the end of the `extensions:` section.
fn add_confext_extension_definition(content: &str, ext_name: &str) -> Result<String> {
    let lines: Vec<&str> = content.lines().collect();
    let ext_section = find_top_level_key(&lines, "extensions")?;
    let ext_indent = leading_spaces(lines[ext_section]);

    // Find the end of the extensions section
    let mut ext_end = ext_section + 1;
    for (i, line) in lines.iter().enumerate().skip(ext_section + 1) {
        if line.trim().is_empty() || line.trim().starts_with('#') {
            ext_end = i + 1;
            continue;
        }
        let indent = leading_spaces(line);
        if indent <= ext_indent {
            break;
        }
        ext_end = i + 1;
    }

    let entry_indent = " ".repeat(ext_indent + 2);
    let sub_indent = " ".repeat(ext_indent + 4);

    let mut result_lines: Vec<String> = lines[..ext_end].iter().map(|l| l.to_string()).collect();

    result_lines.push(String::new());
    result_lines.push(format!("{entry_indent}{ext_name}:"));
    result_lines.push(format!("{sub_indent}types:"));
    result_lines.push(format!("{sub_indent}  - confext"));
    result_lines.push(format!("{sub_indent}version: \"0.1.0\""));
    result_lines.push(format!("{sub_indent}overlay: overlay"));

    // Ensure a blank line separates the new extension from the next section
    if let Some(next_line) = lines.get(ext_end) {
        if !next_line.trim().is_empty() {
            result_lines.push(String::new());
        }
    }

    for line in &lines[ext_end..] {
        result_lines.push(line.to_string());
    }

    let mut out = result_lines.join("\n");
    if content.ends_with('\n') && !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Add an extension name to a runtime's `extensions:` list, inserting before a specific entry.
/// If `before` is not found in the list, appends at the end.
fn add_runtime_extension_entry_before(
    content: &str,
    runtime_name: &str,
    ext_name: &str,
    before: &str,
) -> Result<String> {
    let lines: Vec<&str> = content.lines().collect();
    let rt_section = find_top_level_key(&lines, "runtimes")?;
    let rt_indent = leading_spaces(lines[rt_section]);

    // Find the named runtime
    let mut runtime_line = None;
    for (i, line) in lines.iter().enumerate().skip(rt_section + 1) {
        if line.trim().is_empty() || line.trim().starts_with('#') {
            continue;
        }
        let indent = leading_spaces(line);
        if indent <= rt_indent {
            break;
        }
        let trimmed = line.trim();
        if trimmed.starts_with(&format!("{runtime_name}:")) {
            runtime_line = Some(i);
            break;
        }
    }

    let runtime_line = runtime_line
        .ok_or_else(|| anyhow::anyhow!("Runtime '{runtime_name}' not found in avocado.yaml"))?;
    let runtime_indent = leading_spaces(lines[runtime_line]);

    // Find `extensions:` within this runtime
    let mut ext_list_line = None;
    for (i, line) in lines.iter().enumerate().skip(runtime_line + 1) {
        if line.trim().is_empty() || line.trim().starts_with('#') {
            continue;
        }
        let indent = leading_spaces(line);
        if indent <= runtime_indent {
            break;
        }
        if line.trim() == "extensions:" {
            ext_list_line = Some(i);
            break;
        }
    }

    let ext_list_line = ext_list_line.ok_or_else(|| {
        anyhow::anyhow!("No 'extensions:' list found in runtime '{runtime_name}'")
    })?;
    let list_indent = leading_spaces(lines[ext_list_line]);

    // Find the `before` entry and the end of list
    let mut before_line = None;
    let mut list_end = ext_list_line + 1;
    let mut entry_indent = None;
    for (i, line) in lines.iter().enumerate().skip(ext_list_line + 1) {
        if line.trim().is_empty() || line.trim().starts_with('#') {
            list_end = i + 1;
            continue;
        }
        let indent = leading_spaces(line);
        if indent <= list_indent {
            break;
        }
        if entry_indent.is_none() {
            entry_indent = Some(indent);
        }
        let trimmed = line.trim();
        if trimmed == format!("- {before}") && before_line.is_none() {
            before_line = Some(i);
        }
        list_end = i + 1;
    }

    let entry_indent = entry_indent.unwrap_or(list_indent + 2);
    let indent_str = " ".repeat(entry_indent);
    let new_entry = format!("{indent_str}- {ext_name}");

    let insert_at = before_line.unwrap_or(list_end);

    let mut result_lines: Vec<String> = lines[..insert_at].iter().map(|l| l.to_string()).collect();
    result_lines.push(new_entry);
    for line in &lines[insert_at..] {
        result_lines.push(line.to_string());
    }

    let mut out = result_lines.join("\n");
    if content.ends_with('\n') && !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Check if a runtime's `extensions:` list already includes the given extension name.
fn has_runtime_extension_entry(content: &str, runtime_name: &str, ext_name: &str) -> bool {
    let lines: Vec<&str> = content.lines().collect();
    let rt_section = match find_top_level_key(&lines, "runtimes") {
        Ok(idx) => idx,
        Err(_) => return false,
    };
    let rt_indent = leading_spaces(lines[rt_section]);

    // Find the named runtime
    let mut runtime_line = None;
    for (i, line) in lines.iter().enumerate().skip(rt_section + 1) {
        if line.trim().is_empty() || line.trim().starts_with('#') {
            continue;
        }
        let indent = leading_spaces(line);
        if indent <= rt_indent {
            break;
        }
        let trimmed = line.trim();
        if trimmed.starts_with(&format!("{runtime_name}:")) {
            runtime_line = Some(i);
            break;
        }
    }

    let runtime_line = match runtime_line {
        Some(l) => l,
        None => return false,
    };
    let runtime_indent = leading_spaces(lines[runtime_line]);

    // Find `extensions:` within this runtime
    let mut ext_list_line = None;
    for (i, line) in lines.iter().enumerate().skip(runtime_line + 1) {
        if line.trim().is_empty() || line.trim().starts_with('#') {
            continue;
        }
        let indent = leading_spaces(line);
        if indent <= runtime_indent {
            break;
        }
        if line.trim() == "extensions:" {
            ext_list_line = Some(i);
            break;
        }
    }

    let ext_list_line = match ext_list_line {
        Some(l) => l,
        None => return false,
    };
    let list_indent = leading_spaces(lines[ext_list_line]);

    // Scan list items (lines starting with `- `)
    for line in lines.iter().skip(ext_list_line + 1) {
        if line.trim().is_empty() || line.trim().starts_with('#') {
            continue;
        }
        let indent = leading_spaces(line);
        if indent <= list_indent {
            break;
        }
        let trimmed = line.trim();
        if trimmed == format!("- {ext_name}") {
            return true;
        }
    }

    false
}

/// Add an extension name to a runtime's `extensions:` list.
fn add_runtime_extension_entry(
    content: &str,
    runtime_name: &str,
    ext_name: &str,
) -> Result<String> {
    let lines: Vec<&str> = content.lines().collect();
    let rt_section = find_top_level_key(&lines, "runtimes")?;
    let rt_indent = leading_spaces(lines[rt_section]);

    // Find the named runtime
    let mut runtime_line = None;
    for (i, line) in lines.iter().enumerate().skip(rt_section + 1) {
        if line.trim().is_empty() || line.trim().starts_with('#') {
            continue;
        }
        let indent = leading_spaces(line);
        if indent <= rt_indent {
            break;
        }
        let trimmed = line.trim();
        if trimmed.starts_with(&format!("{runtime_name}:")) {
            runtime_line = Some(i);
            break;
        }
    }

    let runtime_line = runtime_line
        .ok_or_else(|| anyhow::anyhow!("Runtime '{runtime_name}' not found in avocado.yaml"))?;
    let runtime_indent = leading_spaces(lines[runtime_line]);

    // Find `extensions:` within this runtime
    let mut ext_list_line = None;
    for (i, line) in lines.iter().enumerate().skip(runtime_line + 1) {
        if line.trim().is_empty() || line.trim().starts_with('#') {
            continue;
        }
        let indent = leading_spaces(line);
        if indent <= runtime_indent {
            break;
        }
        if line.trim() == "extensions:" {
            ext_list_line = Some(i);
            break;
        }
    }

    let ext_list_line = ext_list_line.ok_or_else(|| {
        anyhow::anyhow!("No 'extensions:' list found in runtime '{runtime_name}'")
    })?;
    let list_indent = leading_spaces(lines[ext_list_line]);

    // Find end of the extensions list
    let mut list_end = ext_list_line + 1;
    let mut entry_indent = None;
    for (i, line) in lines.iter().enumerate().skip(ext_list_line + 1) {
        if line.trim().is_empty() || line.trim().starts_with('#') {
            list_end = i + 1;
            continue;
        }
        let indent = leading_spaces(line);
        if indent <= list_indent {
            break;
        }
        if entry_indent.is_none() {
            entry_indent = Some(indent);
        }
        list_end = i + 1;
    }

    let entry_indent = entry_indent.unwrap_or(list_indent + 2);
    let indent_str = " ".repeat(entry_indent);

    let mut result_lines: Vec<String> = lines[..list_end].iter().map(|l| l.to_string()).collect();

    result_lines.push(format!("{indent_str}- {ext_name}"));

    for line in &lines[list_end..] {
        result_lines.push(line.to_string());
    }

    let mut out = result_lines.join("\n");
    if content.ends_with('\n') && !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Ensure an extension has an `overlay:` field set. If it already has one, return
/// its value. If not, add the field with the given default value and return it.
pub fn ensure_extension_overlay(
    config_path: &Path,
    ext_name: &str,
    default_overlay: &str,
) -> Result<String> {
    let content = std::fs::read_to_string(config_path)
        .with_context(|| format!("Failed to read {}", config_path.display()))?;

    let lines: Vec<&str> = content.lines().collect();
    let ext_section = find_top_level_key(&lines, "extensions")?;
    let ext_indent = leading_spaces(lines[ext_section]);

    // Find the named extension
    let mut ext_line = None;
    for (i, line) in lines.iter().enumerate().skip(ext_section + 1) {
        if line.trim().is_empty() || line.trim().starts_with('#') {
            continue;
        }
        let indent = leading_spaces(line);
        if indent <= ext_indent {
            break;
        }
        let trimmed = line.trim();
        if trimmed.starts_with(&format!("{ext_name}:")) {
            ext_line = Some(i);
            break;
        }
    }

    let ext_line = ext_line
        .ok_or_else(|| anyhow::anyhow!("Extension '{ext_name}' not found in avocado.yaml"))?;
    let ext_def_indent = leading_spaces(lines[ext_line]);

    // Scan the extension's direct children to check for overlay: and find insertion point.
    // Direct children are at ext_def_indent + 2 (the field indent level).
    let field_indent = ext_def_indent + 2;
    let mut last_direct_child_end = ext_line + 1;

    for (i, line) in lines.iter().enumerate().skip(ext_line + 1) {
        if line.trim().is_empty() || line.trim().starts_with('#') {
            continue;
        }
        let indent = leading_spaces(line);
        if indent <= ext_def_indent {
            break; // Left this extension entirely
        }
        if indent == field_indent {
            let trimmed = line.trim();
            if trimmed.starts_with("overlay:") {
                let value = trimmed.trim_start_matches("overlay:").trim();
                let value = value
                    .trim_start_matches('"')
                    .trim_end_matches('"')
                    .trim_start_matches('\'')
                    .trim_end_matches('\'');
                return Ok(value.to_string());
            }
        }
        // Track end of content belonging to this extension (any indent level)
        last_direct_child_end = i + 1;
    }

    // Not found — insert overlay: as a direct child field.
    // Insert right after the last line of content in this extension block.
    let indent_str = " ".repeat(field_indent);
    let insert_at = last_direct_child_end;

    let mut result_lines: Vec<String> = lines[..insert_at].iter().map(|l| l.to_string()).collect();
    result_lines.push(format!("{indent_str}overlay: {default_overlay}"));
    for line in &lines[insert_at..] {
        result_lines.push(line.to_string());
    }

    let mut out = result_lines.join("\n");
    if content.ends_with('\n') && !out.ends_with('\n') {
        out.push('\n');
    }

    std::fs::write(config_path, &out)
        .with_context(|| format!("Failed to write {}", config_path.display()))?;

    Ok(default_overlay.to_string())
}

/// Remove the top-level `connect:` section from avocado.yaml.
///
/// Returns `true` if the section was found and removed.
pub fn remove_connect_fields(config_path: &Path) -> Result<bool> {
    let content = std::fs::read_to_string(config_path)
        .with_context(|| format!("Failed to read {}", config_path.display()))?;

    let (new_content, changed) = remove_connect_fields_in_yaml(&content);

    if changed {
        std::fs::write(config_path, &new_content)
            .with_context(|| format!("Failed to write {}", config_path.display()))?;
    }

    Ok(changed)
}

/// Pure-function core for remove_connect_fields.
fn remove_connect_fields_in_yaml(content: &str) -> (String, bool) {
    let lines: Vec<&str> = content.lines().collect();

    // Find connect: top-level key
    let connect_line = lines.iter().enumerate().find(|(_, line)| {
        let trimmed = line.trim();
        trimmed.starts_with("connect:") && leading_spaces(line) == 0
    });

    let (idx, _) = match connect_line {
        Some(pair) => pair,
        None => return (content.to_string(), false),
    };

    // Find end of connect section (all indented children + trailing blanks)
    let mut section_end = idx + 1;
    for (i, line) in lines.iter().enumerate().skip(idx + 1) {
        if line.trim().is_empty() || line.trim().starts_with('#') {
            section_end = i + 1;
            continue;
        }
        let indent = leading_spaces(line);
        if indent == 0 {
            break;
        }
        section_end = i + 1;
    }

    // Also consume blank lines immediately before connect:
    let mut start = idx;
    while start > 0 && lines[start - 1].trim().is_empty() {
        start -= 1;
    }

    let mut result_lines: Vec<String> = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        if i >= start && i < section_end {
            continue;
        }
        result_lines.push(line.to_string());
    }

    let mut out = result_lines.join("\n");
    if content.ends_with('\n') && !out.ends_with('\n') {
        out.push('\n');
    }

    (out, true)
}

/// Remove the `avocado-ext-connect-config` extension from avocado.yaml.
///
/// Removes the extension definition from the `extensions:` section and
/// the `- avocado-ext-connect-config` entry from the specified runtime's
/// `extensions:` list. Returns `true` if any changes were made.
pub fn remove_connect_config_extension(config_path: &Path, runtime_name: &str) -> Result<bool> {
    let content = std::fs::read_to_string(config_path)
        .with_context(|| format!("Failed to read {}", config_path.display()))?;

    let (new_content, changed) = remove_connect_config_extension_in_yaml(&content, runtime_name)?;

    if changed {
        std::fs::write(config_path, &new_content)
            .with_context(|| format!("Failed to write {}", config_path.display()))?;
    }

    Ok(changed)
}

/// Pure-function core for remove_connect_config_extension.
fn remove_connect_config_extension_in_yaml(
    content: &str,
    runtime_name: &str,
) -> Result<(String, bool)> {
    let mut result = content.to_string();
    let mut changed = false;

    let ext_name = "avocado-ext-connect-config";

    // Step 1: Remove from runtime's extensions list
    if has_runtime_extension_entry(&result, runtime_name, ext_name) {
        result = remove_runtime_extension_entry(&result, runtime_name, ext_name)?;
        changed = true;
    }

    // Step 2: Remove extension definition from extensions: section
    if has_extension_definition(&result, ext_name) {
        result = remove_extension_definition(&result, ext_name)?;
        changed = true;
    }

    Ok((result, changed))
}

/// Remove an extension definition from the top-level `extensions:` section.
fn remove_extension_definition(content: &str, ext_name: &str) -> Result<String> {
    let lines: Vec<&str> = content.lines().collect();
    let ext_section = find_top_level_key(&lines, "extensions")?;
    let ext_indent = leading_spaces(lines[ext_section]);

    // Find the named extension definition
    let mut ext_line = None;
    for (i, line) in lines.iter().enumerate().skip(ext_section + 1) {
        if line.trim().is_empty() || line.trim().starts_with('#') {
            continue;
        }
        let indent = leading_spaces(line);
        if indent <= ext_indent {
            break;
        }
        let trimmed = line.trim();
        if trimmed.starts_with(&format!("{ext_name}:"))
            || trimmed.starts_with(&format!("\"{ext_name}\":"))
        {
            ext_line = Some(i);
            break;
        }
    }

    let ext_line = ext_line
        .ok_or_else(|| anyhow::anyhow!("Extension '{ext_name}' not found in avocado.yaml"))?;
    let ext_def_indent = leading_spaces(lines[ext_line]);

    // Find end of this extension's block (all lines indented deeper)
    let mut block_end = ext_line + 1;
    for (i, line) in lines.iter().enumerate().skip(ext_line + 1) {
        if line.trim().is_empty() || line.trim().starts_with('#') {
            block_end = i + 1;
            continue;
        }
        let indent = leading_spaces(line);
        if indent <= ext_def_indent {
            break;
        }
        block_end = i + 1;
    }

    // Consume blank lines before the extension definition
    let mut start = ext_line;
    while start > 0 && lines[start - 1].trim().is_empty() {
        start -= 1;
    }

    let mut result_lines: Vec<String> = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        if i >= start && i < block_end {
            continue;
        }
        result_lines.push(line.to_string());
    }

    let mut out = result_lines.join("\n");
    if content.ends_with('\n') && !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Remove an extension entry from a runtime's `extensions:` list.
fn remove_runtime_extension_entry(
    content: &str,
    runtime_name: &str,
    ext_name: &str,
) -> Result<String> {
    let lines: Vec<&str> = content.lines().collect();
    let rt_section = find_top_level_key(&lines, "runtimes")?;
    let rt_indent = leading_spaces(lines[rt_section]);

    // Find the named runtime
    let mut runtime_line = None;
    for (i, line) in lines.iter().enumerate().skip(rt_section + 1) {
        if line.trim().is_empty() || line.trim().starts_with('#') {
            continue;
        }
        let indent = leading_spaces(line);
        if indent <= rt_indent {
            break;
        }
        let trimmed = line.trim();
        if trimmed.starts_with(&format!("{runtime_name}:")) {
            runtime_line = Some(i);
            break;
        }
    }

    let runtime_line = runtime_line
        .ok_or_else(|| anyhow::anyhow!("Runtime '{runtime_name}' not found in avocado.yaml"))?;
    let runtime_indent = leading_spaces(lines[runtime_line]);

    // Find extensions: list within this runtime
    let mut ext_list_line = None;
    for (i, line) in lines.iter().enumerate().skip(runtime_line + 1) {
        if line.trim().is_empty() || line.trim().starts_with('#') {
            continue;
        }
        let indent = leading_spaces(line);
        if indent <= runtime_indent {
            break;
        }
        if line.trim() == "extensions:" {
            ext_list_line = Some(i);
            break;
        }
    }

    let ext_list_line = ext_list_line.ok_or_else(|| {
        anyhow::anyhow!("No 'extensions:' list found in runtime '{runtime_name}'")
    })?;
    let list_indent = leading_spaces(lines[ext_list_line]);

    // Find and remove the matching entry
    let target = format!("- {ext_name}");
    let mut result_lines: Vec<String> = Vec::with_capacity(lines.len());

    for (i, line) in lines.iter().enumerate() {
        if i > ext_list_line {
            let indent = leading_spaces(line);
            if !line.trim().is_empty() && !line.trim().starts_with('#') && indent <= list_indent {
                // Past the extensions list — just copy
                result_lines.push(line.to_string());
                continue;
            }
            if line.trim() == target {
                continue; // skip this entry
            }
        }
        result_lines.push(line.to_string());
    }

    let mut out = result_lines.join("\n");
    if content.ends_with('\n') && !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Set connect fields (org, project, server_key) in avocado.yaml.
///
/// If a `connect:` section already exists, updates fields in place.
/// If no `connect:` section exists, appends one at the end of the file.
pub fn set_connect_fields(
    config_path: &Path,
    org: &str,
    project: &str,
    server_key: &str,
) -> Result<()> {
    let content = std::fs::read_to_string(config_path)
        .with_context(|| format!("Failed to read {}", config_path.display()))?;

    let new_content = set_connect_fields_in_yaml(&content, org, project, server_key);

    std::fs::write(config_path, &new_content)
        .with_context(|| format!("Failed to write {}", config_path.display()))?;

    Ok(())
}

fn set_connect_fields_in_yaml(content: &str, org: &str, project: &str, server_key: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();

    // Try to find existing `connect:` top-level key
    let connect_line = lines.iter().enumerate().find(|(_, line)| {
        let trimmed = line.trim();
        trimmed.starts_with("connect:") && leading_spaces(line) == 0
    });

    match connect_line {
        Some((idx, _)) => {
            // Found existing connect: section — replace its fields
            let connect_indent = 2;
            let mut result_lines: Vec<String> = Vec::with_capacity(lines.len() + 3);

            // Copy lines up to and including `connect:`
            for line in &lines[..=idx] {
                result_lines.push(line.to_string());
            }

            // Skip old connect fields (lines indented deeper than connect:)
            let mut skip_end = idx + 1;
            for (i, line) in lines.iter().enumerate().skip(idx + 1) {
                if line.trim().is_empty() || line.trim().starts_with('#') {
                    skip_end = i + 1;
                    continue;
                }
                let indent = leading_spaces(line);
                if indent == 0 {
                    break;
                }
                skip_end = i + 1;
            }

            // Write new connect fields
            let indent = " ".repeat(connect_indent);
            result_lines.push(format!("{indent}org: {org}"));
            result_lines.push(format!("{indent}project: {project}"));
            result_lines.push(format!("{indent}server_key: {server_key}"));

            // Copy remaining lines
            for line in &lines[skip_end..] {
                result_lines.push(line.to_string());
            }

            let mut out = result_lines.join("\n");
            if content.ends_with('\n') && !out.ends_with('\n') {
                out.push('\n');
            }
            out
        }
        None => {
            // No connect: section — append one
            let mut out = content.to_string();
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!(
                "\nconnect:\n  org: {org}\n  project: {project}\n  server_key: {server_key}\n"
            ));
            out
        }
    }
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

    #[test]
    fn test_ensure_connect_extensions_adds_missing() {
        let config = r#"runtimes:
  dev:
    extensions:
      - app
      - config
    packages:
      avocado-runtime: '*'

extensions:
  app:
    types:
      - sysext
    version: "0.1.0"
"#;
        let (result, changed) = ensure_connect_extensions_in_yaml(config, "dev").unwrap();
        assert!(changed);
        assert!(result.contains("avocado-ext-connect-config:"));
        assert!(result.contains("avocado-ext-connect:"));
        assert!(result.contains("avocado-ext-tunnels:"));
        assert!(result.contains("      - avocado-ext-connect-config"));
        assert!(result.contains("      - avocado-ext-connect"));
        assert!(result.contains("      - avocado-ext-tunnels"));
        // avocado-ext-connect-config must appear before avocado-ext-connect in runtime list
        let config_pos = result.find("      - avocado-ext-connect-config").unwrap();
        let connect_pos = result.find("      - avocado-ext-connect\n").unwrap();
        assert!(
            config_pos < connect_pos,
            "avocado-ext-connect-config must precede avocado-ext-connect in runtime extensions"
        );
        // Original entries preserved
        assert!(result.contains("      - app"));
        assert!(result.contains("      - config"));
    }

    #[test]
    fn test_ensure_connect_extensions_already_present() {
        let config = r#"runtimes:
  dev:
    extensions:
      - app
      - avocado-ext-connect-config
      - avocado-ext-connect
      - avocado-ext-tunnels
    packages:
      avocado-runtime: '*'

extensions:
  app:
    types:
      - sysext
    version: "0.1.0"

  avocado-ext-connect-config:
    types:
      - confext
    version: "0.1.0"
    overlay: overlay

  avocado-ext-connect:
    source:
      type: package
      version: "*"

  avocado-ext-tunnels:
    source:
      type: package
      version: "*"
"#;
        let (result, changed) = ensure_connect_extensions_in_yaml(config, "dev").unwrap();
        assert!(!changed);
        assert_eq!(result, config);
    }

    #[test]
    fn test_ensure_connect_extensions_partial() {
        // Only connect is present, config and tunnels are missing
        let config = r#"runtimes:
  dev:
    extensions:
      - app
      - avocado-ext-connect
    packages:
      avocado-runtime: '*'

extensions:
  app:
    types:
      - sysext
    version: "0.1.0"

  avocado-ext-connect:
    source:
      type: package
      version: "*"
"#;
        let (result, changed) = ensure_connect_extensions_in_yaml(config, "dev").unwrap();
        assert!(changed);
        // Config and tunnels should be added
        assert!(result.contains("avocado-ext-connect-config:"));
        assert!(result.contains("avocado-ext-tunnels:"));
        assert!(result.contains("      - avocado-ext-connect-config"));
        assert!(result.contains("      - avocado-ext-tunnels"));
        // avocado-ext-connect-config must appear before avocado-ext-connect in runtime list
        let config_pos = result.find("      - avocado-ext-connect-config").unwrap();
        let connect_pos = result.find("      - avocado-ext-connect\n").unwrap();
        assert!(
            config_pos < connect_pos,
            "avocado-ext-connect-config must precede avocado-ext-connect in runtime extensions"
        );
        // Connect should still be there
        assert!(result.contains("avocado-ext-connect:"));
    }

    #[test]
    fn test_ensure_extension_overlay_existing() {
        let config = r#"extensions:
  avocado-ext-connect-config:
    types:
      - confext
    version: "0.1.0"
    overlay: my-overlay
"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), config).unwrap();
        let result =
            ensure_extension_overlay(tmp.path(), "avocado-ext-connect-config", "overlay").unwrap();
        assert_eq!(result, "my-overlay");
        // File should not be modified
        let after = std::fs::read_to_string(tmp.path()).unwrap();
        assert_eq!(after, config);
    }

    #[test]
    fn test_ensure_extension_overlay_adds_missing() {
        let config = r#"extensions:
  avocado-ext-connect-config:
    types:
      - confext
    version: "0.1.0"
"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), config).unwrap();
        let result =
            ensure_extension_overlay(tmp.path(), "avocado-ext-connect-config", "overlay").unwrap();
        assert_eq!(result, "overlay");
        // File should now contain overlay:
        let after = std::fs::read_to_string(tmp.path()).unwrap();
        assert!(after.contains("overlay: overlay"));
    }

    #[test]
    fn test_remove_connect_fields_present() {
        let config = r#"sdk:
  image: docker.io/avocadolinux/sdk:latest

connect:
  org: 019d2097-a017-733e-a67f-edbafaa7eee9
  project: 019d2097-a11a-798d-9a3e-e8c624495567
  server_key: 463d217f9c292dc4d36cdc86d19a6e7074f7ca71e1a8bcf084d7a1c5df0f5e75
"#;
        let (result, changed) = remove_connect_fields_in_yaml(config);
        assert!(changed);
        assert!(!result.contains("connect:"));
        assert!(!result.contains("server_key"));
        assert!(result.contains("sdk:"));
    }

    #[test]
    fn test_remove_connect_fields_absent() {
        let config = r#"sdk:
  image: docker.io/avocadolinux/sdk:latest
"#;
        let (result, changed) = remove_connect_fields_in_yaml(config);
        assert!(!changed);
        assert_eq!(result, config);
    }

    #[test]
    fn test_remove_connect_fields_at_end_of_file() {
        let config = r#"runtimes:
  dev:
    packages:
      avocado-runtime: '*'

connect:
  org: abc
  project: def
  server_key: ghi
"#;
        let (result, changed) = remove_connect_fields_in_yaml(config);
        assert!(changed);
        assert!(!result.contains("connect:"));
        assert!(result.contains("runtimes:"));
        // Should not have excessive trailing whitespace
        assert!(!result.ends_with("\n\n\n"));
    }

    #[test]
    fn test_remove_connect_config_extension() {
        let config = r#"runtimes:
  dev:
    extensions:
      - avocado-ext-dev
      - avocado-ext-connect-config
      - avocado-ext-connect
      - avocado-ext-tunnels
    packages:
      avocado-runtime: '*'

extensions:
  avocado-ext-dev:
    source:
      type: package
      version: '*'

  avocado-ext-connect-config:
    types:
      - confext
    version: "0.1.0"
    overlay: overlay

  avocado-ext-connect:
    source:
      type: package
      version: "*"

  avocado-ext-tunnels:
    source:
      type: package
      version: "*"
"#;
        let (result, changed) =
            remove_connect_config_extension_in_yaml(config, "dev").unwrap();
        assert!(changed);
        // Extension definition removed
        assert!(!result.contains("avocado-ext-connect-config"));
        // Other extensions preserved
        assert!(result.contains("avocado-ext-connect:"));
        assert!(result.contains("avocado-ext-tunnels:"));
        assert!(result.contains("avocado-ext-dev:"));
        // Runtime list entry removed
        assert!(!result.contains("- avocado-ext-connect-config"));
        assert!(result.contains("- avocado-ext-connect"));
        assert!(result.contains("- avocado-ext-tunnels"));
    }

    #[test]
    fn test_remove_connect_config_extension_not_present() {
        let config = r#"runtimes:
  dev:
    extensions:
      - avocado-ext-dev
      - avocado-ext-connect
      - avocado-ext-tunnels
    packages:
      avocado-runtime: '*'

extensions:
  avocado-ext-dev:
    source:
      type: package
      version: '*'

  avocado-ext-connect:
    source:
      type: package
      version: "*"

  avocado-ext-tunnels:
    source:
      type: package
      version: "*"
"#;
        let (result, changed) =
            remove_connect_config_extension_in_yaml(config, "dev").unwrap();
        assert!(!changed);
        assert_eq!(result, config);
    }
}

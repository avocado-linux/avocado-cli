//! Source-tree version providers for `extensions.<name>.version`.
//!
//! An extension's `version` is an *identity* field: it must resolve to the same
//! string no matter who is reading the config. `{{ env.VAR }}` cannot satisfy
//! that — it binds the value to the caller's environment, so a version declared
//! that way resolves at package time and evaporates (to `""`, with a warning)
//! for anyone consuming the same extension via `source: { type: git | path }`.
//!
//! The mapping form declared here reads the version out of a file **inside the
//! extension's own source tree**, which is present in every consumption mode —
//! the working copy for `path`, the clone for `git`, and the RPM payload for
//! `package`. That makes the version a pure function of the extension.
//!
//! ```yaml
//! # Whole file, trimmed — a plain VERSION file.
//! version:
//!   file: VERSION
//!
//! # One key out of a structured file.
//! version:
//!   file: Cargo.toml
//!   key: package.version
//! ```
//!
//! `key` is the discriminator: absent means "read the file and trim it", with
//! no parsing and no format guessing. Present means parse `file` (format
//! inferred from its extension, or set explicitly) and navigate a dot path.

use anyhow::{bail, Context, Result};
use serde_yaml::Value;
use std::collections::HashMap;
use std::path::{Component, Path};

/// Reads files relative to one extension's source root.
///
/// Exists so version resolution doesn't have to know *how* an extension's tree
/// is reachable — host directory, container path, Docker volume mountpoint, or
/// a throwaway container. [`crate::utils::ext_source_reader::ExtSourceReader`]
/// is the real implementation; tests use an in-memory fake.
pub trait ExtFileReader {
    /// Read a file at `rel` (relative to the extension root).
    fn read_file(&self, rel: &str) -> Result<String>;

    /// Human-readable description of the root, for error messages.
    fn describe(&self) -> String;
}

/// Structured format used to navigate `key` inside `file`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionFileFormat {
    Toml,
    Json,
    Yaml,
}

impl VersionFileFormat {
    fn parse_name(name: &str) -> Option<Self> {
        match name {
            "toml" => Some(Self::Toml),
            "json" => Some(Self::Json),
            "yaml" | "yml" => Some(Self::Yaml),
            _ => None,
        }
    }

    /// Infer from a file extension. Deliberately conservative: an unrecognized
    /// extension yields `None` so the caller can demand an explicit `format:`
    /// rather than guessing at the parse.
    fn infer(file: &str) -> Option<Self> {
        let ext = Path::new(file).extension()?.to_str()?.to_ascii_lowercase();
        Self::parse_name(&ext)
    }
}

/// A declarative `version:` provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionSource {
    /// Path to read, relative to the extension's own root.
    pub file: String,
    /// Dot path into the parsed document. `None` reads the whole file.
    pub key: Option<String>,
    /// Explicit parse format; only meaningful alongside `key`.
    pub format: Option<VersionFileFormat>,
}

impl VersionSource {
    /// Recognize the mapping form of a `version:` value.
    ///
    /// Returns `Ok(None)` when `value` is not a mapping (i.e. the ordinary
    /// string form), so callers can treat this as a probe.
    pub fn from_value(value: &Value) -> Result<Option<Self>> {
        let Some(map) = value.as_mapping() else {
            return Ok(None);
        };

        for key in map.keys() {
            match key.as_str() {
                Some("file") | Some("key") | Some("format") => {}
                Some(other) => bail!(
                    "unknown field '{other}' in a `version:` block \
                     (expected 'file', 'key', or 'format')"
                ),
                None => bail!("non-string field in a `version:` block"),
            }
        }

        let file = match map.get(Value::String("file".into())) {
            Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
            Some(_) => bail!("`version.file` must be a non-empty string"),
            None => bail!("a `version:` block requires a `file` field"),
        };
        validate_relative_path(&file)?;

        let key = match map.get(Value::String("key".into())) {
            Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
            Some(_) => bail!("`version.key` must be a non-empty string"),
            None => None,
        };

        let format = match map.get(Value::String("format".into())) {
            Some(Value::String(s)) => {
                Some(VersionFileFormat::parse_name(s.trim()).with_context(|| {
                    format!("unknown `version.format` '{s}' (expected 'toml', 'json', or 'yaml')")
                })?)
            }
            Some(_) => bail!("`version.format` must be a string"),
            None => None,
        };

        // Without a `key` the file is never parsed, so a `format` would silently
        // do nothing. Reject it rather than let someone believe it took effect.
        if key.is_none() && format.is_some() {
            bail!("`version.format` has no effect without `version.key`; remove it, or add a `key` to parse the file");
        }

        Ok(Some(Self { file, key, format }))
    }

    /// Read and resolve this provider into a semver string.
    pub fn resolve(&self, reader: &dyn ExtFileReader) -> Result<String> {
        let content = reader.read_file(&self.file).with_context(|| {
            format!(
                "could not read `version.file` '{}' under {}",
                self.file,
                reader.describe()
            )
        })?;

        let raw = match &self.key {
            None => content.trim().to_string(),
            Some(key) => self.extract_key(&content, key)?,
        };

        if raw.is_empty() {
            bail!("`version.file` '{}' resolved to an empty string", self.file);
        }

        crate::utils::version::validate_semver(&raw).with_context(|| match &self.key {
            Some(key) => format!(
                "'{}' at key '{key}' in '{}' is not a valid version",
                raw, self.file
            ),
            None => format!("'{}' in '{}' is not a valid version", raw, self.file),
        })?;

        Ok(raw)
    }

    fn extract_key(&self, content: &str, key: &str) -> Result<String> {
        let format = match self.format {
            Some(f) => f,
            None => VersionFileFormat::infer(&self.file).with_context(|| {
                format!(
                    "cannot infer a parse format from `version.file` '{}'; \
                     set `version.format` to 'toml', 'json', or 'yaml'",
                    self.file
                )
            })?,
        };

        let path: Vec<&str> = key.split('.').collect();
        let found = match format {
            VersionFileFormat::Toml => {
                // `toml::from_str`, not `str::parse` — in toml 1.x the latter
                // parses a bare value expression rather than a document.
                let doc: toml::Value = toml::from_str(content)
                    .with_context(|| format!("failed to parse '{}' as TOML", self.file))?;
                nav(&doc, &path, |v, seg| v.get(seg), |v| v.as_str())
            }
            VersionFileFormat::Json => {
                let doc: serde_json::Value = serde_json::from_str(content)
                    .with_context(|| format!("failed to parse '{}' as JSON", self.file))?;
                nav(&doc, &path, |v, seg| v.get(seg), |v| v.as_str())
            }
            VersionFileFormat::Yaml => {
                let doc: Value = serde_yaml::from_str(content)
                    .with_context(|| format!("failed to parse '{}' as YAML", self.file))?;
                nav(&doc, &path, |v, seg| v.get(seg), |v| v.as_str())
            }
        };

        match found {
            KeyLookup::Missing => bail!("'{}' has no key '{key}'", self.file),
            KeyLookup::NotAString => bail!(
                "key '{key}' in '{}' is not a string; a version must be a scalar string",
                self.file
            ),
            KeyLookup::Found(s) => Ok(s.trim().to_string()),
        }
    }
}

enum KeyLookup {
    Found(String),
    Missing,
    NotAString,
}

/// Walk `path` through `doc` using the supplied accessors.
///
/// Generic over the three document types so TOML/JSON/YAML share one traversal
/// instead of three near-identical loops.
fn nav<'a, T, G, S>(doc: &'a T, path: &[&str], get: G, as_str: S) -> KeyLookup
where
    G: Fn(&'a T, &str) -> Option<&'a T>,
    S: Fn(&'a T) -> Option<&'a str>,
{
    let mut current = doc;
    for segment in path {
        match get(current, segment) {
            Some(next) => current = next,
            None => return KeyLookup::Missing,
        }
    }
    match as_str(current) {
        Some(s) => KeyLookup::Found(s.to_string()),
        None => KeyLookup::NotAString,
    }
}

/// An extension may only read inside its own tree. Reject absolute paths and
/// any `..` traversal — for a `git`-sourced extension this is reading out of a
/// shared SDK volume, and for `path` it is reading the user's working copy.
fn validate_relative_path(file: &str) -> Result<()> {
    let path = Path::new(file);
    if path.is_absolute() {
        bail!("`version.file` '{file}' must be relative to the extension root, not absolute");
    }
    for component in path.components() {
        match component {
            Component::ParentDir => bail!(
                "`version.file` '{file}' must stay inside the extension root ('..' is not allowed)"
            ),
            Component::RootDir | Component::Prefix(_) => {
                bail!("`version.file` '{file}' must be relative to the extension root")
            }
            _ => {}
        }
    }
    Ok(())
}

/// Resolve a mapping-form `version:` on a single extension block, replacing it
/// in place with the resolved string.
///
/// Returns the provider that was used, or `None` when `version` is absent or
/// already a plain string. Callers keep the returned provider so packaging can
/// guarantee the referenced file ships in the RPM payload — once the mapping is
/// replaced, the tree no longer records how the version was derived.
pub fn resolve_in_ext_value(
    ext: &mut Value,
    ext_name: &str,
    reader: &dyn ExtFileReader,
) -> Result<Option<VersionSource>> {
    reject_override_blocks(ext, ext_name)?;

    let Some(version) = ext.get("version") else {
        return Ok(None);
    };
    let Some(source) = VersionSource::from_value(version)
        .with_context(|| format!("extension '{ext_name}': invalid `version:` block"))?
    else {
        return Ok(None);
    };

    let resolved = source
        .resolve(reader)
        .with_context(|| format!("extension '{ext_name}': could not resolve `version`"))?;

    if let Some(map) = ext.as_mapping_mut() {
        map.insert(
            Value::String("version".into()),
            Value::String(resolved.clone()),
        );
    }

    Ok(Some(source))
}

/// A `target-<name>:` / `kernel-<spec>:` sub-block may override `version`, but
/// only with a literal. Supporting the mapping form there would mean the RPM
/// packager and this resolver had to agree on two sets of positions; keeping it
/// to the direct child keeps that surface at one.
fn reject_override_blocks(ext: &Value, ext_name: &str) -> Result<()> {
    let Some(map) = ext.as_mapping() else {
        return Ok(());
    };
    for (key, value) in map {
        let Some(name) = key.as_str() else { continue };
        if !(name.starts_with("target-") || name.starts_with("kernel-")) {
            continue;
        }
        if value.get("version").is_some_and(|v| v.is_mapping()) {
            bail!(
                "extension '{ext_name}': `{name}.version` must be a literal version string. \
                 The `{{ file, key }}` form is only supported on the extension's own \
                 `version:` field."
            );
        }
    }
    Ok(())
}

/// Resolve every mapping-form `version:` under `extensions.*`.
///
/// Extensions with no reader are left untouched: an extension whose source tree
/// isn't reachable yet (not fetched) still composes, and fails later with the
/// existing "missing version" diagnostics rather than here.
pub fn resolve_in_config(
    config: &mut Value,
    readers: &HashMap<String, Box<dyn ExtFileReader>>,
) -> Result<HashMap<String, VersionSource>> {
    let mut resolved = HashMap::new();

    let Some(extensions) = config
        .get_mut("extensions")
        .and_then(|e| e.as_mapping_mut())
    else {
        return Ok(resolved);
    };

    for (name_value, ext) in extensions.iter_mut() {
        let Some(name) = name_value.as_str().map(str::to_string) else {
            continue;
        };
        let Some(reader) = readers.get(&name) else {
            continue;
        };
        if let Some(source) = resolve_in_ext_value(ext, &name, reader.as_ref())? {
            resolved.insert(name, source);
        }
    }

    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory extension root.
    struct FakeReader(HashMap<String, String>);

    impl FakeReader {
        fn new(files: &[(&str, &str)]) -> Self {
            Self(
                files
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            )
        }
    }

    impl ExtFileReader for FakeReader {
        fn read_file(&self, rel: &str) -> Result<String> {
            self.0
                .get(rel)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no such file: {rel}"))
        }
        fn describe(&self) -> String {
            "<fake>".to_string()
        }
    }

    fn source(yaml: &str) -> Result<Option<VersionSource>> {
        VersionSource::from_value(&serde_yaml::from_str(yaml).unwrap())
    }

    fn resolve(yaml: &str, files: &[(&str, &str)]) -> Result<String> {
        source(yaml)?
            .expect("expected a mapping form")
            .resolve(&FakeReader::new(files))
    }

    // --- bare `file:` (no key) --------------------------------------------

    #[test]
    fn bare_file_reads_whole_file() {
        let v = resolve("file: VERSION", &[("VERSION", "1.2.3")]).unwrap();
        assert_eq!(v, "1.2.3");
    }

    #[test]
    fn bare_file_trims_trailing_newline() {
        let v = resolve("file: VERSION", &[("VERSION", "1.2.3\n")]).unwrap();
        assert_eq!(v, "1.2.3");
    }

    #[test]
    fn bare_file_trims_surrounding_whitespace() {
        let v = resolve("file: VERSION", &[("VERSION", "  \n 1.2.3 \t\n\n")]).unwrap();
        assert_eq!(v, "1.2.3");
    }

    #[test]
    fn bare_file_that_trims_to_empty_errors() {
        let err = resolve("file: VERSION", &[("VERSION", "  \n\t ")]).unwrap_err();
        assert!(
            err.to_string().contains("empty string"),
            "unexpected: {err:#}"
        );
    }

    /// Without a `key` the file is never parsed, so a structured file is taken
    /// literally rather than being auto-detected. That must fail semver, not
    /// silently produce something.
    #[test]
    fn bare_file_does_not_auto_parse_structured_files() {
        let err = resolve(
            "file: Cargo.toml",
            &[("Cargo.toml", "[package]\nversion = \"1.2.3\"\n")],
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("not a valid version"),
            "unexpected: {err:#}"
        );
    }

    // --- `file:` + `key:` --------------------------------------------------

    #[test]
    fn toml_key_inferred_from_extension() {
        let v = resolve(
            "file: Cargo.toml\nkey: package.version",
            &[(
                "Cargo.toml",
                "[package]\nname = \"x\"\nversion = \"1.0.0-rc.1\"\n",
            )],
        )
        .unwrap();
        assert_eq!(v, "1.0.0-rc.1");
    }

    #[test]
    fn json_key_inferred_from_extension() {
        let v = resolve(
            "file: package.json\nkey: version",
            &[("package.json", r#"{"name":"x","version":"2.1.3"}"#)],
        )
        .unwrap();
        assert_eq!(v, "2.1.3");
    }

    #[test]
    fn yaml_key_inferred_from_extension() {
        let v = resolve(
            "file: meta.yaml\nkey: project.version",
            &[("meta.yaml", "project:\n  version: 0.4.2\n")],
        )
        .unwrap();
        assert_eq!(v, "0.4.2");
    }

    #[test]
    fn explicit_format_overrides_the_extension() {
        let v = resolve(
            "file: versions.txt\nkey: version\nformat: json",
            &[("versions.txt", r#"{"version":"9.9.9"}"#)],
        )
        .unwrap();
        assert_eq!(v, "9.9.9");
    }

    #[test]
    fn unknown_extension_without_format_errors() {
        let err = resolve(
            "file: versions.txt\nkey: version",
            &[("versions.txt", r#"{"version":"9.9.9"}"#)],
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("cannot infer a parse format"),
            "unexpected: {err:#}"
        );
    }

    #[test]
    fn missing_key_errors() {
        let err = resolve(
            "file: Cargo.toml\nkey: package.nope",
            &[("Cargo.toml", "[package]\nversion = \"1.2.3\"\n")],
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("has no key 'package.nope'"),
            "unexpected: {err:#}"
        );
    }

    #[test]
    fn non_scalar_key_errors() {
        let err = resolve(
            "file: Cargo.toml\nkey: package",
            &[("Cargo.toml", "[package]\nversion = \"1.2.3\"\n")],
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("is not a string"),
            "unexpected: {err:#}"
        );
    }

    /// A workspace-inherited `version.workspace = true` is a bool, not a string.
    #[test]
    fn workspace_inherited_version_errors_clearly() {
        let err = resolve(
            "file: Cargo.toml\nkey: package.version",
            &[("Cargo.toml", "[package]\nversion.workspace = true\n")],
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("is not a string"),
            "unexpected: {err:#}"
        );
    }

    // --- shared ------------------------------------------------------------

    #[test]
    fn missing_file_names_the_root() {
        let err = resolve("file: VERSION", &[]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("VERSION") && msg.contains("<fake>"), "{msg}");
    }

    #[test]
    fn non_semver_result_errors() {
        let err = resolve("file: VERSION", &[("VERSION", "nightly")]).unwrap_err();
        assert!(
            format!("{err:#}").contains("not a valid version"),
            "unexpected: {err:#}"
        );
    }

    #[test]
    fn absolute_path_rejected() {
        let err = source("file: /etc/passwd").unwrap_err();
        assert!(err.to_string().contains("not absolute"), "{err:#}");
    }

    #[test]
    fn parent_traversal_rejected() {
        let err = source("file: ../../secrets/VERSION").unwrap_err();
        assert!(err.to_string().contains("'..' is not allowed"), "{err:#}");
    }

    #[test]
    fn nested_relative_path_allowed() {
        let v = resolve(
            "file: crates/app/Cargo.toml\nkey: package.version",
            &[("crates/app/Cargo.toml", "[package]\nversion = \"3.2.1\"\n")],
        )
        .unwrap();
        assert_eq!(v, "3.2.1");
    }

    // --- shape validation ---------------------------------------------------

    #[test]
    fn string_form_is_not_a_mapping() {
        assert_eq!(source("'1.2.3'").unwrap(), None);
    }

    #[test]
    fn missing_file_field_errors() {
        let err = source("key: package.version").unwrap_err();
        assert!(
            err.to_string().contains("requires a `file` field"),
            "{err:#}"
        );
    }

    #[test]
    fn unknown_field_errors() {
        let err = source("file: VERSION\nfrom: cargo").unwrap_err();
        assert!(err.to_string().contains("unknown field 'from'"), "{err:#}");
    }

    #[test]
    fn format_without_key_errors() {
        let err = source("file: VERSION\nformat: toml").unwrap_err();
        assert!(err.to_string().contains("has no effect without"), "{err:#}");
    }

    #[test]
    fn unknown_format_errors() {
        let err = source("file: v.ini\nkey: a.b\nformat: ini").unwrap_err();
        assert!(
            format!("{err:#}").contains("unknown `version.format` 'ini'"),
            "{err:#}"
        );
    }

    // --- in-tree resolution --------------------------------------------------

    #[test]
    fn resolve_in_ext_value_replaces_the_mapping() {
        let mut ext: Value =
            serde_yaml::from_str("version:\n  file: VERSION\nrelease: r0\n").unwrap();
        let reader = FakeReader::new(&[("VERSION", "1.2.3\n")]);

        let source = resolve_in_ext_value(&mut ext, "my-ext", &reader)
            .unwrap()
            .expect("a provider");

        assert_eq!(source.file, "VERSION");
        assert_eq!(ext.get("version").unwrap().as_str(), Some("1.2.3"));
        // Untouched siblings stay put.
        assert_eq!(ext.get("release").unwrap().as_str(), Some("r0"));
    }

    #[test]
    fn resolve_in_ext_value_leaves_string_versions_alone() {
        let mut ext: Value = serde_yaml::from_str("version: '1.2.3'\n").unwrap();
        let reader = FakeReader::new(&[]);
        assert!(resolve_in_ext_value(&mut ext, "my-ext", &reader)
            .unwrap()
            .is_none());
        assert_eq!(ext.get("version").unwrap().as_str(), Some("1.2.3"));
    }

    #[test]
    fn resolve_in_ext_value_errors_name_the_extension() {
        let mut ext: Value = serde_yaml::from_str("version:\n  file: VERSION\n").unwrap();
        let reader = FakeReader::new(&[]);
        let err = resolve_in_ext_value(&mut ext, "avocado-ext-cli", &reader).unwrap_err();
        assert!(
            format!("{err:#}").contains("extension 'avocado-ext-cli'"),
            "{err:#}"
        );
    }

    #[test]
    fn mapping_version_in_a_target_override_is_rejected() {
        let mut ext: Value = serde_yaml::from_str(
            "version: '1.0.0'\ntarget-qemux86-64:\n  version:\n    file: VERSION\n",
        )
        .unwrap();
        let reader = FakeReader::new(&[("VERSION", "1.2.3")]);
        let err = resolve_in_ext_value(&mut ext, "my-ext", &reader).unwrap_err();
        assert!(
            err.to_string().contains("target-qemux86-64.version"),
            "{err:#}"
        );
    }

    #[test]
    fn literal_version_in_a_target_override_is_fine() {
        let mut ext: Value = serde_yaml::from_str(
            "version:\n  file: VERSION\ntarget-qemux86-64:\n  version: '9.9.9'\n",
        )
        .unwrap();
        let reader = FakeReader::new(&[("VERSION", "1.2.3")]);
        resolve_in_ext_value(&mut ext, "my-ext", &reader).unwrap();
        assert_eq!(ext.get("version").unwrap().as_str(), Some("1.2.3"));
    }

    #[test]
    fn resolve_in_config_walks_extensions_and_skips_readerless_ones() {
        let mut config: Value = serde_yaml::from_str(
            "extensions:\n  \
               a:\n    version:\n      file: VERSION\n  \
               b:\n    version: '2.0.0'\n  \
               c:\n    version:\n      file: VERSION\n",
        )
        .unwrap();

        let mut readers: HashMap<String, Box<dyn ExtFileReader>> = HashMap::new();
        readers.insert(
            "a".to_string(),
            Box::new(FakeReader::new(&[("VERSION", "1.1.1")])),
        );
        readers.insert("b".to_string(), Box::new(FakeReader::new(&[])));
        // 'c' has no reader — not fetched yet.

        let resolved = resolve_in_config(&mut config, &readers).unwrap();

        assert_eq!(resolved.len(), 1);
        assert!(resolved.contains_key("a"));
        let exts = config.get("extensions").unwrap();
        assert_eq!(
            exts.get("a").unwrap().get("version").unwrap().as_str(),
            Some("1.1.1")
        );
        assert_eq!(
            exts.get("b").unwrap().get("version").unwrap().as_str(),
            Some("2.0.0")
        );
        assert!(exts.get("c").unwrap().get("version").unwrap().is_mapping());
    }
}

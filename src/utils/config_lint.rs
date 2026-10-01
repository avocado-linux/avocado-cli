//! Warnings for keys in `avocado.yaml` that the CLI ignores.
//!
//! serde drops fields the config structs don't declare, and large parts of the
//! file (`extensions`, overlays, users and groups) are only ever read as raw
//! YAML, so a misspelled key is silently skipped. This walks the user's own file
//! against `schemas/avocado-config.json` and reports every key the schema does
//! not describe. It warns and never fails: a key an older CLI ignored must not
//! start breaking a build.
//!
//! The walker understands the subset of JSON Schema that file uses: `$ref`,
//! `properties`, `patternProperties`, `additionalProperties`, `items`, and
//! `anyOf`/`oneOf`. It adds two keywords of its own:
//! - `x-avocado-target-overrides`: a bare target-name key is a legacy
//!   per-target override of the enclosing block. A key counts when it names a
//!   known target or its value sets one of the block's own fields.
//! - `x-avocado-warning`: the key parses but does not do what it looks like.

use serde_json::Value as Json;
use serde_yaml::Value as Yaml;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const SCHEMA_SOURCE: &str = include_str!("../../schemas/avocado-config.json");

fn schema() -> &'static Json {
    static SCHEMA: OnceLock<Json> = OnceLock::new();
    SCHEMA.get_or_init(|| serde_json::from_str(SCHEMA_SOURCE).expect("embedded schema is JSON"))
}

/// Print the ignored-key warnings for one config file, once per process.
///
/// Commands load the same file many times, so the guard keys on the
/// canonical path (`./avocado.yaml` and `avocado.yaml` are both defaults).
/// A file that doesn't parse is skipped; the loader reports that error.
pub fn warn_ignored_keys_once(config_path: &Path, content: &str) {
    static WARNED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    let key = std::fs::canonicalize(config_path).unwrap_or_else(|_| config_path.to_path_buf());
    {
        let Ok(mut warned) = WARNED.get_or_init(Default::default).lock() else {
            return;
        };
        if !warned.insert(key) {
            return;
        }
    }
    let Ok(doc) = serde_yaml::from_str::<Yaml>(content) else {
        return;
    };
    for warning in ignored_keys(&doc) {
        crate::utils::output::print_warning_stderr(&format!(
            "{}: {warning}",
            config_path.display()
        ));
    }
}

/// Describe every key in `doc` that the CLI ignores, one message per key.
pub fn ignored_keys(doc: &Yaml) -> Vec<String> {
    let root = schema();
    let mut walker = Walker {
        root,
        targets: known_targets(root, doc),
        config_refs: config_refs(doc),
        warnings: Vec::new(),
    };
    walker.walk(root, doc, &mut Vec::new());
    walker.warnings
}

struct Walker<'a> {
    root: &'a Json,
    /// Keys a `x-avocado-target-overrides` block accepts as bare target names.
    targets: HashSet<String>,
    /// Top-level keys the file reads back through `{{ config.<key> }}`, which
    /// makes a key of the user's own a legitimate interpolation variable.
    config_refs: HashSet<String>,
    warnings: Vec<String>,
}

impl<'a> Walker<'a> {
    fn walk(&mut self, schema: &'a Json, value: &Yaml, path: &mut Vec<String>) {
        let schema = self.resolve(schema);
        if let Some(branches) = schema.get("anyOf").or_else(|| schema.get("oneOf")) {
            // Parsing both `anyOf` and `oneOf` the same way is deliberate: the
            // CLI never requires exactly one shape to match, it infers one.
            let branches: Vec<&Json> = branches
                .as_array()
                .into_iter()
                .flatten()
                .map(|b| self.resolve(b))
                .collect();
            if let Some((branch, entry_fields)) = self.pick_branch(&branches, value) {
                match value {
                    Yaml::Mapping(map) if !is_union(branch) => {
                        self.check_mapping(branch, map, path, entry_fields)
                    }
                    _ => self.walk(branch, value, path),
                }
            }
            return;
        }
        match value {
            Yaml::Mapping(map) => self.check_mapping(schema, map, path, None),
            Yaml::Sequence(items) => {
                let Some(item_schema) = schema.get("items") else {
                    return;
                };
                for (i, item) in items.iter().enumerate() {
                    let last = path.pop().unwrap_or_default();
                    path.push(format!("{last}[{i}]"));
                    self.walk(item_schema, item, path);
                    path.pop();
                    path.push(last);
                }
            }
            _ => {}
        }
    }

    /// Choose the `anyOf` branch the CLI would read `value` as.
    ///
    /// For a mapping this mirrors the CLI's own shape inference: a branch
    /// whose `enum` field matches wins (the tagged extension `source`), then a
    /// branch that declares one of the mapping's keys as a field (the
    /// singleton form of `rootfs`, `kernel` and friends), then a map branch
    /// (their named-entry form). When the named-entry form wins over a
    /// singleton branch, the singleton's fields come back too, so an entry
    /// that sets none of them can be flagged as a probable misspelling.
    fn pick_branch(
        &self,
        branches: &[&'a Json],
        value: &Yaml,
    ) -> Option<(&'a Json, Option<&'a serde_json::Map<String, Json>>)> {
        let kind = yaml_kind(value);
        let fits: Vec<&'a Json> = branches
            .iter()
            .copied()
            .filter(|b| accepts_kind(b, kind))
            .collect();
        let Yaml::Mapping(map) = value else {
            return fits.first().map(|b| (*b, None));
        };
        let props = |b: &'a Json| b.get("properties").and_then(Json::as_object);

        // A templated tag only resolves at interpolation, so the shape is unknown.
        if fits.iter().any(|b| tag(b, value) == Tag::Templated) {
            return None;
        }
        if let Some(branch) = fits.iter().copied().find(|b| tag(b, value) == Tag::Matches) {
            return Some((branch, None));
        }
        let singleton = fits.iter().copied().find(|b| props(b).is_some());
        if let Some(branch) = singleton {
            // A tagged field only counts when its tag matches: the CLI reads
            // `rootfs.source` as a path source only with `type: path`, and
            // otherwise as an entry named `source`.
            let claims = map.iter().any(|(k, v)| {
                key_string(k)
                    .and_then(|k| props(branch).unwrap().get(&k))
                    .is_some_and(|spec| tag(self.resolve(spec), v) != Tag::Mismatch)
            });
            if claims {
                return Some((branch, None));
            }
        }
        let named = fits.iter().copied().find(|b| {
            props(b).is_none() && b.get("additionalProperties").is_some_and(Json::is_object)
        });
        match (named, singleton) {
            (Some(named), singleton) => Some((named, singleton.and_then(props))),
            (None, Some(singleton)) => Some((singleton, None)),
            (None, None) => fits.first().map(|b| (*b, None)),
        }
    }

    fn check_mapping(
        &mut self,
        schema: &'a Json,
        map: &serde_yaml::Mapping,
        path: &mut Vec<String>,
        entry_fields: Option<&'a serde_json::Map<String, Json>>,
    ) {
        let schema = self.resolve(schema);
        let props = schema.get("properties").and_then(Json::as_object);
        let patterns = schema.get("patternProperties").and_then(Json::as_object);
        let additional = schema.get("additionalProperties");
        let closed = additional == Some(&Json::Bool(false));
        let target_overrides = schema.get("x-avocado-target-overrides") == Some(&Json::Bool(true));

        for (key, value) in map {
            let Some(key) = key_string(key) else { continue };
            path.push(key.clone());
            if let Some(spec) = props.and_then(|p| p.get(&key)) {
                match spec.get("x-avocado-warning").and_then(Json::as_str) {
                    Some(message) => self.warn(format!("'{}' {message}", path.join("."))),
                    None => self.walk(spec, value, path),
                }
            } else if let Some(spec) = patterns.and_then(|p| matching_pattern(p, &key)) {
                self.walk(spec, value, path);
            } else if target_overrides
                && (self.targets.contains(&key) || sets_a_field(value, props))
            {
                self.walk(schema, value, path);
            } else if let Some(entry) = additional.filter(|a| a.is_object()) {
                match entry_fields {
                    Some(fields) if sets_no_field(value, fields) => {
                        self.warn_named_entry(path, &key, fields)
                    }
                    _ => self.walk(entry, value, path),
                }
            } else if closed
                && !key.contains("{{")
                && !(path.len() == 1 && self.config_refs.contains(&key))
            {
                let hint = props
                    .and_then(|p| suggest(&key, p.keys()))
                    .map(|s| format!("; did you mean '{s}'?"))
                    .unwrap_or_default();
                self.warn(format!("unknown key '{}' is ignored{hint}", path.join(".")));
            }
            path.pop();
        }
    }

    fn warn_named_entry(
        &mut self,
        path: &[String],
        key: &str,
        fields: &serde_json::Map<String, Json>,
    ) {
        let section = path[..path.len() - 1].join(".");
        let hint = suggest(key, fields.keys())
            .map(|s| format!("; did you mean the field '{s}'?"))
            .unwrap_or_default();
        self.warn(format!(
            "'{}' sets no {section} fields, so it is read as a named {section} entry{hint}",
            path.join(".")
        ));
    }

    fn warn(&mut self, message: String) {
        self.warnings.push(message);
    }

    fn resolve(&self, mut schema: &'a Json) -> &'a Json {
        while let Some(pointer) = schema.get("$ref").and_then(Json::as_str) {
            let Some(target) = pointer.strip_prefix('#').and_then(|p| self.root.pointer(p)) else {
                break;
            };
            schema = target;
        }
        schema
    }
}

/// Whether `value` is a mapping that sets one of `fields`: the shape of a
/// bare-name per-target override. The target it names may only exist on the
/// command line or after interpolation, so the name alone can't decide.
fn sets_a_field(value: &Yaml, fields: Option<&serde_json::Map<String, Json>>) -> bool {
    let (Yaml::Mapping(map), Some(fields)) = (value, fields) else {
        return false;
    };
    map.keys()
        .filter_map(key_string)
        .any(|k| fields.contains_key(&k))
}

/// Whether an entry of a named-entry section looks like a misspelled field of
/// the singleton form instead: a non-empty value that sets none of its fields.
fn sets_no_field(value: &Yaml, fields: &serde_json::Map<String, Json>) -> bool {
    match value {
        Yaml::Null => false,
        Yaml::Mapping(map) => {
            !map.is_empty()
                && !map
                    .keys()
                    .filter_map(key_string)
                    .any(|k| fields.contains_key(&k) || k.starts_with("target-"))
        }
        _ => true,
    }
}

#[derive(PartialEq)]
enum Tag {
    /// The schema has no `enum` field to discriminate on.
    Untagged,
    Matches,
    /// Missing, or not one of the allowed values.
    Mismatch,
    Templated,
}

/// How `value` fares against a schema's `enum` fields, such as `type: path`.
fn tag(schema: &Json, value: &Yaml) -> Tag {
    let tagged = schema
        .get("properties")
        .and_then(Json::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(field, spec)| Some((field, spec.get("enum")?.as_array()?)));
    let mut result = Tag::Untagged;
    for (field, allowed) in tagged {
        match value.get(field.as_str()).and_then(Yaml::as_str) {
            Some(actual) if actual.contains("{{") => return Tag::Templated,
            Some(actual) if allowed.iter().any(|a| a.as_str() == Some(actual)) => {
                result = Tag::Matches
            }
            _ if result == Tag::Untagged => result = Tag::Mismatch,
            _ => {}
        }
    }
    result
}

fn is_union(schema: &Json) -> bool {
    schema.get("anyOf").is_some() || schema.get("oneOf").is_some()
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Object,
    Array,
    Other,
}

fn yaml_kind(value: &Yaml) -> Kind {
    match value {
        Yaml::Mapping(_) => Kind::Object,
        Yaml::Sequence(_) => Kind::Array,
        _ => Kind::Other,
    }
}

/// Whether a (resolved) branch could describe a value of this kind. Only
/// mappings and sequences carry keys, so scalars never need a precise match.
fn accepts_kind(branch: &Json, kind: Kind) -> bool {
    let types: Vec<&str> = match branch.get("type") {
        Some(Json::String(t)) => vec![t.as_str()],
        Some(Json::Array(ts)) => ts.iter().filter_map(Json::as_str).collect(),
        _ => return true,
    };
    match kind {
        Kind::Object => types.contains(&"object"),
        Kind::Array => types.contains(&"array"),
        Kind::Other => !types.iter().all(|t| *t == "object" || *t == "array"),
    }
}

fn key_string(key: &Yaml) -> Option<String> {
    match key {
        Yaml::String(s) => Some(s.clone()),
        Yaml::Number(n) => Some(n.to_string()),
        Yaml::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn matching_pattern<'a>(
    patterns: &'a serde_json::Map<String, Json>,
    key: &str,
) -> Option<&'a Json> {
    patterns.iter().find_map(|(pattern, spec)| {
        regex::Regex::new(pattern)
            .ok()
            .filter(|re| re.is_match(key))
            .map(|_| spec)
    })
}

/// The closest known key within a small, case-insensitive edit distance.
/// Renamed keys (`ext`, `runtime`) are too far for this and carry their own
/// `x-avocado-warning` in the schema instead.
fn suggest<'a>(key: &str, known: impl Iterator<Item = &'a String>) -> Option<&'a str> {
    let limit = (key.chars().count() / 3).max(1);
    known
        .filter_map(|candidate| {
            let distance = strsim::osa_distance(&key.to_lowercase(), &candidate.to_lowercase());
            (distance <= limit).then_some((distance, candidate.as_str()))
        })
        .min()
        .map(|(_, candidate)| candidate)
}

/// Target names a bare-key override may use: the ones Avocado OS ships (the
/// schema's `target` enum) plus any the file itself declares.
fn known_targets(root: &Json, doc: &Yaml) -> HashSet<String> {
    let mut targets: HashSet<String> = root
        .pointer("/definitions/target/anyOf/0/enum")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| t.as_str().map(str::to_string))
        .collect();
    let mut add = |value: Option<&Yaml>| match value {
        Some(Yaml::String(s)) => {
            targets.insert(s.clone());
        }
        Some(Yaml::Sequence(items)) => {
            targets.extend(items.iter().filter_map(Yaml::as_str).map(str::to_string));
        }
        _ => {}
    };
    add(doc.get("default_target"));
    add(doc.get("supported_targets"));
    if let Some(Yaml::Mapping(runtimes)) = doc.get("runtimes") {
        for runtime in runtimes.values() {
            add(runtime.get("target"));
            add(runtime.get("targets"));
        }
    }
    targets
}

/// Top-level keys named by a `{{ config.<key>... }}` template anywhere in the file.
fn config_refs(doc: &Yaml) -> HashSet<String> {
    fn visit(value: &Yaml, re: &regex::Regex, out: &mut HashSet<String>) {
        match value {
            Yaml::String(s) => out.extend(re.captures_iter(s).map(|c| c[1].to_string())),
            Yaml::Sequence(items) => items.iter().for_each(|v| visit(v, re, out)),
            Yaml::Mapping(map) => map.iter().for_each(|(k, v)| {
                visit(k, re, out);
                visit(v, re, out);
            }),
            _ => {}
        }
    }
    let re = regex::Regex::new(r"\{\{\s*config\.([A-Za-z0-9_-]+)").expect("valid regex");
    let mut out = HashSet::new();
    visit(doc, &re, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn warnings(yaml: &str) -> Vec<String> {
        ignored_keys(&serde_yaml::from_str(yaml).unwrap())
    }

    #[test]
    fn embedded_schema_parses() {
        assert!(schema().get("definitions").is_some());
    }

    /// The field names serde's derive accepts for `T`, aliases included.
    fn serde_fields<'de, T: serde::Deserialize<'de>>() -> Vec<&'static str> {
        struct Fields(Vec<&'static str>);
        #[derive(Debug)]
        struct Stop;
        impl std::fmt::Display for Stop {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("stop")
            }
        }
        impl std::error::Error for Stop {}
        impl serde::de::Error for Stop {
            fn custom<M: std::fmt::Display>(_: M) -> Self {
                Stop
            }
        }
        impl<'de> serde::Deserializer<'de> for &mut Fields {
            type Error = Stop;
            fn deserialize_any<V: serde::de::Visitor<'de>>(self, _: V) -> Result<V::Value, Stop> {
                Err(Stop)
            }
            fn deserialize_struct<V: serde::de::Visitor<'de>>(
                self,
                _: &'static str,
                fields: &'static [&'static str],
                _: V,
            ) -> Result<V::Value, Stop> {
                self.0.extend(fields);
                Err(Stop)
            }
            serde::forward_to_deserialize_any! {
                bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
                bytes byte_buf option unit unit_struct newtype_struct seq tuple
                tuple_struct map enum identifier ignored_any
            }
        }
        let mut fields = Fields(Vec::new());
        let _ = T::deserialize(&mut fields);
        fields.0
    }

    /// Each typed config struct's fields, keyed by the schema object that
    /// describes it.
    fn typed_config_fields() -> Vec<(&'static str, Vec<&'static str>)> {
        use crate::utils::config::*;
        use crate::utils::container_dev::config::{
            ContainerDevConfig, ContainerDevImage, RegistryConfig,
        };
        vec![
            ("", serde_fields::<Config>()),
            (
                "/definitions/runtimeConfig",
                serde_fields::<RuntimeConfig>(),
            ),
            ("/definitions/sdkConfig", serde_fields::<SdkConfig>()),
            (
                "/definitions/compileConfig",
                serde_fields::<CompileConfig>(),
            ),
            (
                "/definitions/packageConfig",
                serde_fields::<PackageConfig>(),
            ),
            (
                "/definitions/splitPackageConfig",
                serde_fields::<SplitPackageConfig>(),
            ),
            ("/definitions/imageConfig", serde_fields::<ImageConfig>()),
            ("/definitions/kernelConfig", serde_fields::<KernelConfig>()),
            (
                "/definitions/permissionsConfig",
                serde_fields::<PermissionsConfig>(),
            ),
            (
                "/definitions/provisionProfileConfig",
                serde_fields::<ProvisionProfileConfig>(),
            ),
            ("/definitions/distroConfig", serde_fields::<DistroConfig>()),
            (
                "/definitions/distroRepoConfig",
                serde_fields::<DistroRepoConfig>(),
            ),
            ("/definitions/repoDef", serde_fields::<RepoDef>()),
            ("/definitions/varConfig", serde_fields::<VarConfig>()),
            (
                "/definitions/subvolumeEntry/anyOf/2",
                serde_fields::<SubvolumeConfig>(),
            ),
            (
                "/definitions/signingConfig",
                serde_fields::<SigningConfig>(),
            ),
            (
                "/definitions/connectConfig",
                serde_fields::<ConnectConfig>(),
            ),
            (
                "/definitions/containerDevConfig",
                serde_fields::<ContainerDevConfig>(),
            ),
            (
                "/definitions/containerDevConfig/properties/images/items",
                serde_fields::<ContainerDevImage>(),
            ),
            (
                "/definitions/containerDevConfig/properties/registry",
                serde_fields::<RegistryConfig>(),
            ),
        ]
    }

    /// Every field a typed config struct accepts must be in the schema, or the
    /// CLI would warn that a key it reads is ignored. A new config struct
    /// belongs in [`typed_config_fields`].
    #[test]
    fn schema_describes_every_typed_config_field() {
        let mut missing = Vec::new();
        for (pointer, fields) in typed_config_fields() {
            assert!(
                !fields.is_empty(),
                "no serde fields captured for {pointer:?}"
            );
            let props = schema()
                .pointer(pointer)
                .and_then(|s| s.get("properties"))
                .and_then(Json::as_object)
                .unwrap_or_else(|| panic!("schema has no properties at {pointer:?}"));
            missing.extend(
                fields
                    .into_iter()
                    .filter(|f| !props.contains_key(*f))
                    .map(|f| format!("{pointer}/properties/{f}")),
            );
        }
        assert_eq!(missing, Vec::<String>::new());
    }

    /// The configs this repo ships and tests against must not warn. The
    /// fixtures left out carry ignored keys or are not whole config files.
    #[test]
    fn repo_configs_have_no_warnings() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let fixtures = root.join("tests/fixtures/configs");
        let mut files = vec![
            root.join("avocado.yaml"),
            root.join("docs/container-dev/lab/avocado.yaml"),
        ];
        files.extend(
            [
                "minimal.yaml",
                "with-nested-target-config.yaml",
                "with-overlay-merge.yaml",
                "with-overlay-opaque.yaml",
                "with-overlay.yaml",
                "with-permissions.yaml",
                "with-signing-keys.yaml",
                "with-sysext.yaml",
                "with-users.yaml",
            ]
            .iter()
            .map(|f| fixtures.join(f)),
        );
        for file in &files {
            let doc: Yaml = serde_yaml::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
            assert_eq!(
                ignored_keys(&doc),
                Vec::<String>::new(),
                "{}",
                file.display()
            );
        }
        let template = include_str!("../../configs/default.yaml").replace("{target}", "qemux86-64");
        assert_eq!(
            warnings(&template),
            Vec::<String>::new(),
            "configs/default.yaml"
        );
    }

    #[test]
    fn a_clean_config_has_no_warnings() {
        let yaml = r#"
default_target: qemux86-64
supported_targets: ["qemux86-64"]
distro: { release: 2024, channel: edge }
runtimes:
  dev:
    extensions: [app, { avocado-ext-dev: { enabled: false } }]
    packages: { avocado-runtime: "*" }
    target-qemux86-64:
      packages: { extra: "*" }
extensions:
  app:
    types: [sysext]
    version: "0.1.0"
    overlay: { dir: overlay, mode: opaque }
    packages:
      my-app: { compile: my-app, install: install.sh }
  "avocado-bsp-{{ avocado.target.board }}":
    source: { type: package, version: "*" }
  remote:
    source: { type: git, url: "https://example.com/x.git", ref: main }
rootfs: { permissions: dev }
permissions:
  dev:
    users: { root: { password: "" } }
sdk:
  image: "docker.io/avocadolinux/sdk:{{ config.distro.release }}"
  container_args: "--privileged"
  packages: { "libstdc++": "*" }
"#;
        assert_eq!(warnings(yaml), Vec::<String>::new());
    }

    #[test]
    fn flags_unknown_keys_with_suggestions() {
        let yaml = "sdkk: {}\nruntimes:\n  dev:\n    extentions: [app]\n";
        assert_eq!(
            warnings(yaml),
            vec![
                "unknown key 'sdkk' is ignored; did you mean 'sdk'?",
                "unknown key 'runtimes.dev.extentions' is ignored; did you mean 'extensions'?",
            ]
        );
    }

    #[test]
    fn names_the_replacement_for_renamed_keys() {
        assert_eq!(
            warnings("ext: {}\nextensions:\n  app: { sysext: true }\n"),
            vec![
                "'ext' is an old name for 'extensions' and is no longer read; rename it to 'extensions'",
                "'extensions.app.sysext' is no longer read; list the image types under 'types', e.g. 'types: [sysext]'",
            ]
        );
    }

    #[test]
    fn offers_no_suggestion_when_nothing_is_close() {
        assert_eq!(
            warnings("extensions:\n  app: { files: [a] }\n"),
            vec!["unknown key 'extensions.app.files' is ignored"]
        );
    }

    #[test]
    fn flags_a_misspelled_singleton_read_as_a_named_entry() {
        let yaml = "rootfs:\n  pakages: { curl: \"*\" }\npermissions:\n  usres: { root: {} }\n";
        assert_eq!(
            warnings(yaml),
            vec![
                "'rootfs.pakages' sets no rootfs fields, so it is read as a named rootfs entry; did you mean the field 'packages'?",
                "'permissions.usres' sets no permissions fields, so it is read as a named permissions entry; did you mean the field 'users'?",
            ]
        );
    }

    #[test]
    fn a_real_named_entry_is_fine() {
        let yaml =
            "kernel:\n  yocto-6-6: { package: kernel, version: \"6.6.*\" }\nrootfs:\n  empty: {}\n";
        assert_eq!(warnings(yaml), Vec::<String>::new());
    }

    #[test]
    fn a_misspelled_field_beside_a_real_one_is_flagged_in_place() {
        assert_eq!(
            warnings("rootfs:\n  permissions: dev\n  filesytem: erofs\n"),
            vec!["unknown key 'rootfs.filesytem' is ignored; did you mean 'filesystem'?"]
        );
    }

    #[test]
    fn picks_the_source_branch_by_type() {
        let yaml = "extensions:\n  a:\n    source: { type: path, path: ../a, url: x }\n";
        assert_eq!(
            warnings(yaml),
            vec!["unknown key 'extensions.a.source.url' is ignored"]
        );
    }

    #[test]
    fn accepts_bare_target_overrides_only_where_the_cli_reads_them() {
        let yaml = "extensions:\n  app:\n    raspberrypi4: { packages: {} }\nrepos:\n  acme:\n    raspberrypi4: {}\n";
        assert_eq!(
            warnings(yaml),
            vec!["unknown key 'repos.acme.raspberrypi4' is ignored"]
        );
    }

    #[test]
    fn reports_keys_that_parse_but_misbehave() {
        assert_eq!(
            warnings("sdk:\n  dependencies: { gcc: \"*\" }\n"),
            vec!["'sdk.dependencies' is an old name for 'packages', and 'avocado sdk install' only installs 'packages'; rename it to 'packages'"]
        );
    }

    #[test]
    fn flags_fields_read_only_on_the_top_level_block() {
        let yaml = "kernel:\n  lts: { package: k, source: { type: path, path: k } }\nrootfs:\n  default: { packages: {}, overlay: o }\nruntimes:\n  dev:\n    rootfs: { permissions: dev, packages: {} }\n";
        assert_eq!(
            warnings(yaml),
            vec![
                "unknown key 'kernel.lts.source' is ignored",
                "unknown key 'rootfs.default.overlay' is ignored",
                "unknown key 'runtimes.dev.rootfs.packages' is ignored",
            ]
        );
    }

    #[test]
    fn a_templated_source_type_is_not_guessed() {
        let yaml = "source_kind: git\nextensions:\n  app:\n    source:\n      type: \"{{ config.source_kind }}\"\n      url: https://example.com/app.git\n";
        assert_eq!(warnings(yaml), Vec::<String>::new());
    }

    #[test]
    fn source_is_a_path_source_only_with_type_path() {
        let named = "kernel:\n  source: { compile: linux, install: install.sh }\n";
        assert_eq!(warnings(named), Vec::<String>::new());
        let path = "rootfs:\n  source: { type: path, path: fragments/rootfs }\n";
        assert_eq!(warnings(path), Vec::<String>::new());
    }

    #[test]
    fn a_bare_override_for_a_custom_target_is_recognised_by_shape() {
        let yaml = "default_target: \"{{ env.PROJECT_TARGET }}\"\nsdk:\n  acme-board: { image: x, imgae: y }\nruntimes:\n  dev:\n    pakages: { curl: \"*\" }\n";
        assert_eq!(
            warnings(yaml),
            vec![
                "unknown key 'sdk.acme-board.imgae' is ignored; did you mean 'image'?",
                "unknown key 'runtimes.dev.pakages' is ignored; did you mean 'packages'?",
            ]
        );
    }

    #[test]
    fn flags_image_fields_a_target_override_does_not_carry() {
        let yaml = "rootfs:\n  packages: {}\n  target-qemux86-64:\n    post_install: a.sh\n    packages: { curl: \"*\" }\n";
        assert_eq!(
            warnings(yaml),
            vec!["unknown key 'rootfs.target-qemux86-64.packages' is ignored"]
        );
    }

    #[test]
    fn allows_top_level_keys_used_as_interpolation_variables() {
        let yaml = "base_image: foo\nunused: 1\nsdk:\n  image: \"{{ config.base_image }}\"\n";
        assert_eq!(warnings(yaml), vec!["unknown key 'unused' is ignored"]);
    }

    #[test]
    fn indexes_sequence_paths() {
        let yaml = "extensions:\n  app:\n    docker_images:\n      - { image: redis, tag: \"7\", digest: x }\n";
        assert_eq!(
            warnings(yaml),
            vec!["unknown key 'extensions.app.docker_images[0].digest' is ignored"]
        );
    }

    #[test]
    fn templated_keys_are_names_not_fields() {
        let yaml = "runtimes:\n  dev:\n    \"{{ avocado.target }}-thing\": {}\n";
        assert_eq!(warnings(yaml), Vec::<String>::new());
    }
}

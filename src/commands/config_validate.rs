//! `avocado config validate` — check an avocado.yaml without building.
//!
//! Errors are everything that makes the file wrong: a YAML parse error, every
//! violation of `schemas/avocado-config.json`, and the loader's own checks.
//! Warnings are the keys the CLI ignores (`config_lint`). avocado-mcp and
//! editors call this, so they report exactly what the CLI reports.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value as Json};
use std::io::Read;
use std::sync::OnceLock;

use crate::utils::config::Config;
use crate::utils::config_lint;
use crate::utils::output::{print_error, print_success, print_warning_stderr, OutputLevel};
use crate::utils::output_format::{emit_json_object, JsonOutputGuard, OutputFormat};

pub struct ConfigValidateCommand {
    /// Path to avocado.yaml, or `-` for stdin.
    pub config_path: String,
    pub output: OutputFormat,
}

/// One error: a JSON pointer into the config (empty for the whole file) and
/// what is wrong there.
#[derive(Debug, PartialEq)]
pub struct ConfigError {
    pub path: String,
    pub message: String,
}

#[derive(Debug, Default)]
pub struct Report {
    pub errors: Vec<ConfigError>,
    pub warnings: Vec<String>,
}

impl ConfigValidateCommand {
    pub async fn execute(&self) -> Result<()> {
        // Interpolation warns about unset env vars through `print_warning`,
        // which writes to stdout unless JSON output is active.
        let _json = self.output.is_json().then(JsonOutputGuard::enable);

        let content = if self.config_path == "-" {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("Failed to read the config from stdin")?;
            buf
        } else {
            std::fs::read_to_string(&self.config_path)
                .with_context(|| format!("Failed to read config file: {}", self.config_path))?
        };

        let report = validate(&content);
        let ok = report.errors.is_empty();
        if self.output.is_json() {
            let errors: Vec<Json> = report
                .errors
                .iter()
                .map(|e| json!({ "path": e.path, "message": e.message }))
                .collect();
            emit_json_object(&json!({
                "ok": ok,
                "errors": errors,
                "warnings": report.warnings,
            }));
        } else {
            let name = if self.config_path == "-" {
                "<stdin>"
            } else {
                &self.config_path
            };
            for e in &report.errors {
                let message = if e.path.is_empty() {
                    format!("{name}: {}", e.message)
                } else {
                    format!("{name}: {}: {}", dotted(&e.path), e.message)
                };
                print_error(&message, OutputLevel::Normal);
            }
            for w in &report.warnings {
                print_warning_stderr(&format!("{name}: {w}"));
            }
            if ok {
                print_success(&format!("{name} is valid"), OutputLevel::Normal);
            }
        }
        if !ok {
            bail!("{} error(s) in the config", report.errors.len());
        }
        Ok(())
    }
}

/// Validate config text. Never fails: every problem is in the report.
pub fn validate(content: &str) -> Report {
    let mut report = Report::default();
    let doc: serde_yaml::Value = match serde_yaml::from_str(content) {
        Ok(doc) => doc,
        Err(e) => {
            report.errors.push(ConfigError {
                path: String::new(),
                message: format!("YAML parse error: {e}"),
            });
            return report;
        }
    };
    report.warnings = config_lint::ignored_keys(&doc);

    // Check the value the loader deserializes, after interpolation, so a
    // `{{ }}` template in a boolean or number field is not a type error. When
    // interpolation fails there is no such value: the raw template would only
    // give a misleading schema error, so the interpolation error is the one.
    let mut interpolated = doc.clone();
    if let Err(e) = crate::utils::interpolation::interpolate_config(&mut interpolated, None, None) {
        report.errors.push(ConfigError {
            path: String::new(),
            message: e.root_cause().to_string(),
        });
        return report;
    }
    match serde_json::to_value(&interpolated) {
        Ok(instance) => {
            for e in validator().iter_errors(&instance) {
                report.errors.extend(schema_errors(&e));
            }
        }
        Err(e) => report.errors.push(ConfigError {
            path: String::new(),
            message: format!("not representable as JSON: {e}"),
        }),
    }

    // The loader's checks, for the rules the schema cannot express. It stops
    // at its first error and gives no path, and that error mostly repeats a
    // schema error, so it is shown only when the schema found none.
    if report.errors.is_empty() {
        if let Err(e) = Config::load_from_yaml_str(content) {
            report.errors.push(ConfigError {
                path: String::new(),
                message: e.root_cause().to_string(),
            });
        }
    }

    // Messages quote the value after interpolation, which for `{{ env.X }}`
    // is whatever the environment holds, a token included. Show the template
    // the user wrote instead: it names the line to fix and reveals nothing.
    // Paths go through the interpolated keys, so they get the same treatment.
    // Every env value a template reads is hidden too, for the places where
    // the two documents do not pair.
    let mut templated = Vec::new();
    templated_values(&doc, &interpolated, &mut templated);
    let mut env = Vec::new();
    env_values(&doc, &mut env);
    templated.extend(
        env.iter()
            .map(|(value, template)| (serde_yaml::Value::String(value.clone()), template.clone())),
    );
    for e in &mut report.errors {
        e.message = show_templates(&e.message, &templated);
        e.path = source_pointer(&doc, &interpolated, &e.path, &env);
    }
    report
}

/// Every scalar that interpolation changed, as `(expanded, template)`, found
/// by walking the raw and the interpolated document together.
fn templated_values(
    raw: &serde_yaml::Value,
    expanded: &serde_yaml::Value,
    out: &mut Vec<(serde_yaml::Value, String)>,
) {
    use serde_yaml::Value;
    match (raw, expanded) {
        (Value::String(template), value) if template.contains("{{") && raw != value => {
            out.push((value.clone(), template.clone()));
        }
        (Value::Sequence(r), Value::Sequence(x)) => {
            for (r, x) in r.iter().zip(x) {
                templated_values(r, x, out);
            }
        }
        (Value::Mapping(r), Value::Mapping(x)) => {
            for ((rk, rv), (xk, xv)) in paired_entries(r, x) {
                templated_values(rk, xk, out);
                templated_values(rv, xv, out);
            }
        }
        _ => {}
    }
}

/// The entries of a mapping before and after interpolation, as
/// `((raw key, raw value), (expanded key, expanded value))`. A templated key
/// is removed and reinserted at the end, so entries pair by key first and
/// the leftover templated keys pair in order. When interpolation changed the
/// number of entries (two keys expanded to one, or the distro override was
/// folded in), the leftovers cannot be told apart and are not paired.
#[allow(clippy::type_complexity)]
fn paired_entries<'a>(
    r: &'a serde_yaml::Mapping,
    x: &'a serde_yaml::Mapping,
) -> Vec<(
    (&'a serde_yaml::Value, &'a serde_yaml::Value),
    (&'a serde_yaml::Value, &'a serde_yaml::Value),
)> {
    let mut pairs = Vec::new();
    let mut raw_left = Vec::new();
    for (k, rv) in r {
        match x.get(k) {
            Some(xv) => pairs.push(((k, rv), (k, xv))),
            None => raw_left.push((k, rv)),
        }
    }
    if r.len() == x.len() {
        let raw_left = raw_left
            .into_iter()
            .filter(|(k, _)| k.as_str().is_some_and(|k| k.contains("{{")));
        let x_left = x.iter().filter(|(k, _)| !r.contains_key(*k));
        pairs.extend(raw_left.zip(x_left));
    }
    pairs
}

/// Each env var a template in `doc` reads, as `(value, "{{ env.NAME }}")`.
/// Unset and empty values are left out: they cannot reveal anything.
fn env_values(doc: &serde_yaml::Value, out: &mut Vec<(String, String)>) {
    use serde_yaml::Value;
    static TEMPLATE: OnceLock<regex::Regex> = OnceLock::new();
    let template = TEMPLATE.get_or_init(|| regex::Regex::new(r"\{\{\s*([^}]+)\s*\}\}").unwrap());
    match doc {
        Value::String(s) => {
            for capture in template.captures_iter(s) {
                let Some(name) = capture[1].trim().strip_prefix("env.") else {
                    continue;
                };
                match std::env::var(name) {
                    Ok(value) if !value.is_empty() => {
                        out.push((value, format!("{{{{ env.{name} }}}}")));
                    }
                    _ => {}
                }
            }
        }
        Value::Sequence(items) => {
            for item in items {
                env_values(item, out);
            }
        }
        Value::Mapping(map) => {
            for (k, v) in map {
                env_values(k, out);
                env_values(v, out);
            }
        }
        Value::Tagged(tagged) => env_values(&tagged.value, out),
        _ => {}
    }
}

/// `message` with each expanded value replaced by its template. JSON-quoted
/// forms (how the schema errors print values) are replaced for every value;
/// bare forms (the loader's messages) only for values of four or more
/// characters, so a short value cannot rewrite unrelated words.
fn show_templates(message: &str, templated: &[(serde_yaml::Value, String)]) -> String {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for (value, template) in templated {
        let bare = match value {
            serde_yaml::Value::String(s) => s.clone(),
            serde_yaml::Value::Number(n) => n.to_string(),
            serde_yaml::Value::Bool(b) => b.to_string(),
            _ => continue,
        };
        if bare.is_empty() {
            continue;
        }
        let quoted = serde_json::to_string(&bare).unwrap_or_default();
        pairs.push((quoted, serde_json::to_string(template).unwrap_or_default()));
        if bare.chars().count() >= 4 {
            pairs.push((bare, template.clone()));
        }
    }
    // Longest first, so a value that contains another is replaced whole.
    pairs.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
    let mut out = message.to_string();
    for (from, to) in pairs {
        out = out.replace(&from, &to);
    }
    out
}

/// `pointer`, a path into the interpolated config, as the path into the
/// config the user wrote: a templated key is shown as its template. Where a
/// mapping does not pair (`paired_entries`), the walk stops, and from there
/// on each segment only has its env values hidden (`hide_env`).
fn source_pointer(
    raw: &serde_yaml::Value,
    expanded: &serde_yaml::Value,
    pointer: &str,
    env: &[(String, String)],
) -> String {
    let mut at = Some((raw, expanded));
    let mut out = String::new();
    for segment in pointer.split('/').skip(1) {
        let key = segment.replace("~1", "/").replace("~0", "~");
        let shown = match at.and_then(|(r, x)| source_step(r, x, &key)) {
            Some((shown, r, x)) => {
                at = Some((r, x));
                shown
            }
            None => {
                at = None;
                hide_env(&key, env)
            }
        };
        out.push('/');
        out.push_str(&shown.replace('~', "~0").replace('/', "~1"));
    }
    out
}

/// One step of `source_pointer`: the key the user wrote for `key`, and the
/// raw and expanded values under it. A mapping that changed its number of
/// entries may pair a key with another entry's value, so it is not walked.
fn source_step<'a>(
    raw: &'a serde_yaml::Value,
    expanded: &'a serde_yaml::Value,
    key: &str,
) -> Option<(String, &'a serde_yaml::Value, &'a serde_yaml::Value)> {
    use serde_yaml::Value;
    let text = |k: &Value| match k {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    };
    match (raw, expanded) {
        (Value::Mapping(r), Value::Mapping(x)) if r.len() == x.len() => paired_entries(r, x)
            .into_iter()
            .find(|(_, (xk, _))| text(xk).as_deref() == Some(key))
            .and_then(|((rk, rv), (_, xv))| Some((text(rk)?, rv, xv))),
        (Value::Sequence(r), Value::Sequence(x)) => {
            let i: usize = key.parse().ok()?;
            Some((key.to_string(), r.get(i)?, x.get(i)?))
        }
        _ => None,
    }
}

/// `key` with each env value in it replaced by its template. As in
/// `show_templates`, a value of fewer than four characters is replaced only
/// when it is the whole key.
fn hide_env(key: &str, env: &[(String, String)]) -> String {
    let mut env: Vec<_> = env
        .iter()
        .filter(|(value, _)| value == key || value.chars().count() >= 4)
        .collect();
    env.sort_by_key(|(value, _)| std::cmp::Reverse(value.len()));
    let mut out = key.to_string();
    for (value, template) in env {
        out = out.replace(value.as_str(), template);
    }
    out
}

/// A JSON pointer as the dotted key path the lint warnings use.
fn dotted(pointer: &str) -> String {
    pointer
        .trim_start_matches('/')
        .split('/')
        .map(|part| part.replace("~1", "/").replace("~0", "~"))
        .collect::<Vec<_>>()
        .join(".")
}

/// One schema error as the errors to show. An `anyOf` or `oneOf` that no
/// branch matches is shown as the errors of the branch the loader would read
/// the value as (`branch_score`). Among equal branches it is the one that
/// came closest: its errors go deepest into the config, then it has the
/// fewest. The error's own message only says that no branch matched.
fn schema_errors(e: &jsonschema::ValidationError) -> Vec<ConfigError> {
    use jsonschema::error::ValidationErrorKind as Kind;
    match e.kind() {
        Kind::AnyOf { context } | Kind::OneOfNotValid { context } => {
            let root = config_lint::schema();
            let branches = root
                .pointer(e.schema_path().as_str())
                .and_then(Json::as_array);
            let score = |i: usize| {
                branches
                    .and_then(|b| b.get(i))
                    .map_or(0, |b| branch_score(root, b, e.instance()))
            };
            let depth = |errors: &Vec<jsonschema::ValidationError>| {
                errors
                    .iter()
                    .map(|e| e.instance_path().as_str().matches('/').count())
                    .max()
                    .unwrap_or(0)
            };
            let closest = context
                .iter()
                .enumerate()
                .filter(|(_, errors)| !errors.is_empty())
                .max_by(|(i, a), (j, b)| {
                    score(*i)
                        .cmp(&score(*j))
                        .then(depth(a).cmp(&depth(b)))
                        .then(b.len().cmp(&a.len()))
                });
            match closest {
                Some((_, errors)) => errors.iter().flat_map(schema_errors).collect(),
                None => vec![leaf_error(e)],
            }
        }
        _ => vec![leaf_error(e)],
    }
}

/// How strongly the loader reads `value` as this branch, the same shape
/// inference as `config_lint`: a matching `const`/`enum` tag (the extension
/// `source` type) wins, then each key the branch declares as a field (the
/// one-block form of `rootfs`, `kernel` and friends). A map of named entries
/// declares no fields, so it wins only when no key is a field.
fn branch_score(root: &Json, branch: &Json, value: &Json) -> usize {
    let branch = branch
        .get("$ref")
        .and_then(Json::as_str)
        .and_then(|r| r.strip_prefix('#'))
        .and_then(|pointer| root.pointer(pointer))
        .unwrap_or(branch);
    let (Some(fields), Some(map)) = (
        branch.get("properties").and_then(Json::as_object),
        value.as_object(),
    ) else {
        return 0;
    };
    let tagged = fields.iter().any(|(key, field)| {
        let Some(v) = map.get(key) else { return false };
        field.get("const") == Some(v)
            || field
                .get("enum")
                .and_then(Json::as_array)
                .is_some_and(|e| e.contains(v))
    });
    let declared = map.keys().filter(|k| fields.contains_key(*k)).count();
    if tagged {
        1000 + declared
    } else {
        declared
    }
}

fn leaf_error(e: &jsonschema::ValidationError) -> ConfigError {
    use jsonschema::error::ValidationErrorKind as Kind;
    let message = match e.kind() {
        // The schema uses `false` for keys that exclude each other, such as
        // kernel `cmdline` and `cmdline_extra`.
        Kind::FalseSchema => "must not be set together with another key in this block".to_string(),
        // Only the one-block form keeps `additionalProperties: false`
        // (`keep_singleton_keys`).
        Kind::AdditionalProperties { unexpected } => format!(
            "is one block, because it sets a field, so {} cannot be named entries. Use one form",
            unexpected
                .iter()
                .map(|k| format!("'{k}'"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => e.to_string(),
    };
    ConfigError {
        path: e.instance_path().to_string(),
        message,
    }
}

/// The schema validator. `additionalProperties: false` is removed, because
/// the CLI ignores an unknown key and warns about it (`config_lint`); it is
/// not an error. The one exception is where the rule picks the branch
/// (`keep_singleton_keys`).
fn validator() -> &'static jsonschema::Validator {
    static VALIDATOR: OnceLock<jsonschema::Validator> = OnceLock::new();
    VALIDATOR.get_or_init(|| {
        let original = config_lint::schema();
        let mut schema = original.clone();
        relax(&mut schema);
        keep_singleton_keys(original, &mut schema);
        jsonschema::validator_for(&schema).expect("embedded schema compiles")
    })
}

fn relax(node: &mut Json) {
    match node {
        Json::Object(map) => {
            if map.get("additionalProperties") == Some(&Json::Bool(false)) {
                map.remove("additionalProperties");
            }
            map.values_mut().for_each(relax);
        }
        Json::Array(items) => items.iter_mut().for_each(relax),
        _ => {}
    }
}

/// Put the unknown-key rule back where it picks the branch. `rootfs`,
/// `initramfs`, `kernel` and `permissions` take one block or a map of named
/// blocks. The loader reads the mapping as one block when a key is a field of
/// that block, and then every other key is an error, except `target-<name>`
/// and `kernel-<spec>` overrides (`named_or_single_deserializer`). Without the
/// rule, the one-block branch accepts any mapping, so named entries are never
/// checked.
///
/// Only the branch site gets the rule. The named entries themselves stay
/// relaxed, so their unknown keys are `config_lint` warnings.
fn keep_singleton_keys(original: &Json, node: &mut Json) {
    match node {
        Json::Array(items) => items
            .iter_mut()
            .for_each(|n| keep_singleton_keys(original, n)),
        Json::Object(map) => {
            map.values_mut()
                .for_each(|n| keep_singleton_keys(original, n));
            let key = if map.contains_key("anyOf") {
                "anyOf"
            } else {
                "oneOf"
            };
            if let Some(Json::Array(branches)) = map.get_mut(key) {
                set_singleton_rule(original, branches);
            }
        }
        _ => {}
    }
}

fn set_singleton_rule(original: &Json, branches: &mut [Json]) {
    let resolve = |node: &Json| -> Json {
        let target = node
            .get("$ref")
            .and_then(Json::as_str)
            .and_then(|r| r.strip_prefix('#'))
            .and_then(|pointer| original.pointer(pointer));
        target.unwrap_or(node).clone()
    };
    // The named-entry form: a map with no fields of its own.
    let is_named = |b: &Json| {
        let b = resolve(b);
        b.get("properties").is_none() && b.get("additionalProperties").is_some_and(Json::is_object)
    };
    if !branches.iter().any(is_named) {
        return;
    }
    let mut claims = None;
    for branch in branches.iter_mut() {
        let single = resolve(branch);
        let Some(fields) = single.get("properties").and_then(Json::as_object) else {
            continue;
        };
        if single.get("additionalProperties") != Some(&Json::Bool(false)) {
            continue;
        }
        let claim = field_claim(&resolve, fields);
        let Json::Object(b) = branch else { continue };
        let mut properties: serde_json::Map<String, Json> = fields
            .keys()
            .map(|k| (k.clone(), Json::Bool(true)))
            .collect();
        if let Some(Json::Object(own)) = b.get("properties") {
            properties.extend(own.clone());
        }
        let mut patterns = serde_json::Map::new();
        patterns.insert("^(target|kernel)-".to_string(), Json::Bool(true));
        if let Some(Json::Object(own)) = b.get("patternProperties") {
            patterns.extend(own.clone());
        }
        b.insert("type".to_string(), json!("object"));
        b.insert("properties".to_string(), Json::Object(properties));
        b.insert("patternProperties".to_string(), Json::Object(patterns));
        b.insert("additionalProperties".to_string(), Json::Bool(false));
        b.insert("allOf".to_string(), json!([claim.clone()]));
        claims = Some(claim);
    }
    // The loader reads a mapping with no field key as named entries, even
    // one whose keys start with `target-`, and a mapping with a field key as
    // one block. So each form must reject the other's mappings.
    let Some(claims) = claims else { return };
    for branch in branches.iter_mut() {
        if is_named(branch) {
            if let Json::Object(b) = branch {
                b.insert("not".to_string(), claims.clone());
            }
        }
    }
}

/// A schema that matches a mapping the loader reads as one block: it sets
/// one of `fields`. A field that is a tagged object, such as
/// `source: {type: path}`, counts only when the tag matches. Otherwise the
/// loader reads `source` as the name of an entry.
fn field_claim(resolve: &dyn Fn(&Json) -> Json, fields: &serde_json::Map<String, Json>) -> Json {
    let branches: Vec<Json> = fields
        .iter()
        .map(|(name, spec)| {
            let spec = resolve(spec);
            let tags: serde_json::Map<String, Json> = spec
                .get("properties")
                .and_then(Json::as_object)
                .into_iter()
                .flatten()
                .filter(|(_, p)| p.get("enum").is_some() || p.get("const").is_some())
                .map(|(k, p)| {
                    let rule = p
                        .get("const")
                        .map(|c| json!({ "const": c }))
                        .unwrap_or_else(|| json!({ "enum": p["enum"] }));
                    (k.clone(), rule)
                })
                .collect();
            if tags.is_empty() {
                json!({ "required": [name] })
            } else {
                let required: Vec<&String> = tags.keys().collect();
                json!({
                    "required": [name],
                    "properties": { name: { "type": "object", "required": required, "properties": tags } },
                })
            }
        })
        .collect();
    json!({ "anyOf": branches })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn errors(yaml: &str) -> Vec<String> {
        validate(yaml)
            .errors
            .into_iter()
            .map(|e| format!("{} {}", e.path, e.message))
            .collect()
    }

    const MARKER: &str = "AVOCADO_LEAK_MARKER_0123456789";

    /// Run `f` with `AVOCADO_TEST_VALIDATE_MARKER` set to a marker value that
    /// stands in for a secret.
    fn with_marker<T>(f: impl FnOnce() -> T) -> T {
        std::env::set_var("AVOCADO_TEST_VALIDATE_MARKER", MARKER);
        let out = f();
        std::env::remove_var("AVOCADO_TEST_VALIDATE_MARKER");
        out
    }

    /// A value read from the environment never appears in a message; the
    /// template the user wrote does, so the message still names the line.
    #[test]
    #[serial_test::serial]
    fn an_env_value_is_shown_as_its_template_not_its_value() {
        let errors = with_marker(|| {
            errors(
                "source_date_epoch: \"{{ env.AVOCADO_TEST_VALIDATE_MARKER }}\"\nruntimes:\n  dev:\n    extensions: []\n",
            )
        });
        assert!(!errors.iter().any(|e| e.contains(MARKER)), "{errors:?}");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("{{ env.AVOCADO_TEST_VALIDATE_MARKER }}")),
            "{errors:?}"
        );
    }

    /// An error on a parent prints the whole value, children included.
    #[test]
    #[serial_test::serial]
    fn an_env_value_inside_a_wrong_typed_parent_is_not_shown() {
        let errors = with_marker(|| {
            errors(
                "runtimes:\n  dev:\n    extensions:\n      app: \"{{ env.AVOCADO_TEST_VALIDATE_MARKER }}\"\n",
            )
        });
        assert!(!errors.is_empty());
        assert!(!errors.iter().any(|e| e.contains(MARKER)), "{errors:?}");
    }

    /// A templated key is renamed by interpolation; its value still pairs.
    #[test]
    #[serial_test::serial]
    fn an_env_value_under_a_templated_key_is_not_shown() {
        let errors = with_marker(|| {
            errors(
                "default_target: qemux86-64\nruntimes:\n  dev:\n    extensions: []\nextensions:\n  \"app-{{ avocado.target }}\":\n    types: \"{{ env.AVOCADO_TEST_VALIDATE_MARKER }}\"\n",
            )
        });
        assert!(!errors.is_empty());
        assert!(!errors.iter().any(|e| e.contains(MARKER)), "{errors:?}");
    }

    /// An error path goes through the interpolated keys; a key read from the
    /// environment is shown as its template there too.
    #[test]
    #[serial_test::serial]
    fn an_env_value_as_a_key_is_not_shown_in_the_path() {
        for (yaml, path) in [
            (
                "runtimes:\n  \"{{ env.AVOCADO_TEST_VALIDATE_MARKER }}\":\n    extensions: 5\n",
                "/runtimes/{{ env.AVOCADO_TEST_VALIDATE_MARKER }}/extensions",
            ),
            (
                "runtimes:\n  \"dev-{{ env.AVOCADO_TEST_VALIDATE_MARKER }}\":\n    extensions: 5\n",
                "/runtimes/dev-{{ env.AVOCADO_TEST_VALIDATE_MARKER }}/extensions",
            ),
            (
                "runtimes:\n  \"dev/{{ env.AVOCADO_TEST_VALIDATE_MARKER }}\":\n    extensions: 5\n",
                "/runtimes/dev~1{{ env.AVOCADO_TEST_VALIDATE_MARKER }}/extensions",
            ),
        ] {
            let report = with_marker(|| validate(yaml));
            assert!(!report.errors.is_empty(), "{yaml}");
            for e in &report.errors {
                assert!(!e.path.contains(MARKER), "{e:?}");
                assert!(!e.message.contains(MARKER), "{e:?}");
            }
            assert!(
                report.errors.iter().any(|e| e.path == path),
                "{:?}",
                report.errors
            );
        }
    }

    /// Only the templated key is shown as its template, not a literal key
    /// further down that has the same text as the env value.
    #[test]
    #[serial_test::serial]
    fn a_literal_key_equal_to_an_env_value_is_kept() {
        std::env::set_var("AVOCADO_TEST_VALIDATE_KEY", "extensions");
        let report =
            validate("runtimes:\n  \"{{ env.AVOCADO_TEST_VALIDATE_KEY }}\":\n    extensions: 5\n");
        std::env::remove_var("AVOCADO_TEST_VALIDATE_KEY");
        let paths: Vec<_> = report.errors.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            ["/runtimes/{{ env.AVOCADO_TEST_VALIDATE_KEY }}/extensions"]
        );
    }

    /// When templated keys expand to a key that is already there, one entry
    /// overwrites the other, so the raw and the expanded entries no longer
    /// pair. The env values in the surviving entry are still hidden.
    #[test]
    #[serial_test::serial]
    fn env_values_under_colliding_keys_are_not_shown() {
        let survivor =
            "    extensions:\n      - \"{{ env.AVOCADO_TEST_VALIDATE_MARKER }}\": {enabled: 5}\n";
        for yaml in [
            format!("runtimes:\n  dev:\n    extensions: []\n  \"{{{{ env.AVOCADO_TEST_VALIDATE_KEY }}}}\":\n{survivor}"),
            format!("runtimes:\n  \"{{{{ env.AVOCADO_TEST_VALIDATE_KEY }}}}\":\n    extensions: []\n  \"{{{{env.AVOCADO_TEST_VALIDATE_KEY}}}}\":\n{survivor}"),
        ] {
            std::env::set_var("AVOCADO_TEST_VALIDATE_KEY", "dev");
            let report = with_marker(|| validate(&yaml));
            std::env::remove_var("AVOCADO_TEST_VALIDATE_KEY");
            assert!(!report.errors.is_empty(), "{yaml}");
            for e in &report.errors {
                assert!(!e.path.contains(MARKER), "{e:?}");
                assert!(!e.message.contains(MARKER), "{e:?}");
            }
        }
    }

    /// The distro override is folded in before the check, so `distro` has
    /// keys the user did not write there. They are not templates.
    #[test]
    fn a_folded_distro_key_keeps_its_path() {
        let report =
            validate("default_target: qemux86-64\ndistro:\n  target-qemux86-64:\n    channel: 5\n");
        let paths: Vec<_> = report.errors.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["/distro/channel"]);
    }

    #[test]
    fn a_short_value_does_not_rewrite_unrelated_words() {
        let templated = vec![(serde_yaml::Value::String("is".into()), "{{ env.X }}".into())];
        assert_eq!(
            show_templates("5 is not of type \"is\"", &templated),
            "5 is not of type \"{{ env.X }}\""
        );
    }

    #[test]
    fn dotted_paths_unescape_pointer_segments() {
        assert_eq!(
            dotted("/runtimes/dev/extensions"),
            "runtimes.dev.extensions"
        );
        assert_eq!(dotted("/repos/a~1b/x~0y"), "repos.a/b.x~y");
    }

    #[test]
    fn the_init_template_is_valid() {
        let template = include_str!("../../configs/default.yaml").replace("{target}", "qemux86-64");
        let report = validate(&template);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    #[test]
    fn a_parse_error_is_the_only_error() {
        let report = validate("default_target: [\n");
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].message.starts_with("YAML parse error"));
    }

    #[test]
    fn a_wrong_type_is_an_error_the_loader_misses() {
        let errs = errors("default_target: qemux86-64\nruntimes:\n  dev:\n    extensions: 5\n");
        assert!(
            errs.iter()
                .any(|e| e.starts_with("/runtimes/dev/extensions ")),
            "{errs:?}"
        );
    }

    #[test]
    fn an_unknown_key_is_a_warning_not_an_error() {
        let report = validate("default_target: qemux86-64\nrutimes: {}\n");
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("did you mean 'runtimes'"));
    }

    #[test]
    fn a_mixed_block_is_one_error_on_the_block() {
        let errs = errors("kernel: {cmdline: a, other: {cmdline: b}}\n");
        assert_eq!(
            errs,
            ["/kernel is one block, because it sets a field, so 'other' cannot be named entries. Use one form"]
        );
    }

    #[test]
    fn exclusive_keys_are_errors_on_both_keys() {
        let errs = errors("kernel: {cmdline: a, cmdline_extra: b}\n");
        assert_eq!(
            errs,
            [
                "/kernel/cmdline_extra must not be set together with another key in this block",
                "/kernel/cmdline must not be set together with another key in this block",
            ]
        );
    }

    #[test]
    fn a_rule_only_the_loader_knows_is_reported() {
        // The loader rejects a recovery key without an encrypted /var. Once
        // the schema encodes it too, the schema error is reported instead.
        let report = validate(
            "default_target: qemux86-64\nruntimes:\n  dev:\n    var: {recovery: master}\n",
        );
        assert!(!report.errors.is_empty());
    }

    // The cases below came from avocado-mcp's validate-yaml tests, which
    // moved here when the MCP started to call `config validate`.

    const BASE: &str =
        "distro: {release: 2024, channel: edge}\nruntimes:\n  dev:\n    extensions: [app]\n";

    #[test]
    fn valid_configs_have_no_errors_or_warnings() {
        for (name, yaml) in [
            ("git source", format!("{BASE}extensions:\n  app:\n    source: {{type: git, url: https://x/y.git, ref: main}}\n")),
            ("path source", format!("{BASE}extensions:\n  app:\n    source: {{type: path, path: ../app}}\n")),
            ("version from a file", format!("{BASE}extensions:\n  app:\n    types: [sysext]\n    version: {{file: Cargo.toml, key: package.version, format: toml}}\n")),
            ("runtime extension in object form", "distro: {release: 2024, channel: edge}\nruntimes:\n  dev:\n    extensions:\n      - app: {enabled: false}\n".to_string()),
            ("distro.repo as a repos: name", "distro: {release: 2024, channel: edge, repo: mirror, feeds: [acme]}\nrepos:\n  mirror: {url: https://m}\n  acme: {org: acme}\nruntimes: {dev: {extensions: [a]}}\n".to_string()),
            ("extension-only config", "supported_targets: '*'\nextensions:\n  foo: {types: [sysext], version: \"1.0.0\"}\n".to_string()),
            ("target overrides in a one-block rootfs", "rootfs:\n  packages: {}\n  target-raspberrypi4: { image: { type: kab } }\n".to_string()),
        ] {
            let report = validate(&yaml);
            assert!(report.errors.is_empty(), "{name}: {:?}", report.errors);
            assert!(report.warnings.is_empty(), "{name}: {:?}", report.warnings);
        }
    }

    #[test]
    fn schema_errors_point_at_the_key() {
        for (yaml, path) in [
            ("kernel: {cmdline: a, cmdline_extra: b}\n", "/kernel/cmdline_extra"),
            (
                "runtimes:\n  dev:\n    extensions: [a]\n    var: {encrypt: true, hardware: bogus}\n",
                "/runtimes/dev/var/hardware",
            ),
            // A bad named entry must not pass as an unknown key of the
            // one-block form.
            ("permissions:\n  dev:\n    users: [root]\n", "/permissions/dev/users"),
            ("rootfs:\n  prod:\n    permissions: 123\n", "/rootfs/prod/permissions"),
            ("kernel:\n  lts:\n    cmdline: 5\n", "/kernel/lts/cmdline"),
        ] {
            let report = validate(yaml);
            assert!(
                report.errors.iter().any(|e| e.path == path),
                "{path}: {:?}",
                report.errors
            );
        }
    }

    #[test]
    fn a_field_key_makes_one_block_and_its_fields_are_checked() {
        // `users` is a field, so this is one block, not an entry named
        // `users`. The loader keeps users as raw YAML and would not notice.
        let errs = errors("permissions: {users: {root: {uid: wrong}}}\n");
        assert!(
            errs.iter()
                .any(|e| e.starts_with("/permissions/users/root/uid ")),
            "{errs:?}"
        );
    }

    #[test]
    fn no_field_key_makes_named_entries_even_with_a_target_prefix() {
        // No field key, so `target-dev` is the name of an entry.
        let errs = errors("permissions: {target-dev: {users: {root: {uid: wrong}}}}\n");
        assert!(
            errs.iter()
                .any(|e| e.starts_with("/permissions/target-dev/users/root/uid ")),
            "{errs:?}"
        );
    }

    #[test]
    fn source_is_a_field_only_with_type_path() {
        let path = validate("rootfs:\n  source: { type: path, path: fragments/rootfs }\n");
        assert!(path.errors.is_empty(), "{:?}", path.errors);
        // Without `type: path`, `source` is an entry name.
        let named = validate("kernel:\n  source: { compile: linux, install: install.sh }\n");
        assert!(named.errors.is_empty(), "{:?}", named.errors);
    }

    #[test]
    fn an_interpolation_error_is_the_error() {
        let report = validate(
            "default_target: qemux86-64\nruntimes:\n  dev:\n    var: {encrypt: true, hardware: \"{{ config.missing }}\"}\n",
        );
        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
        assert_eq!(report.errors[0].path, "");
        assert!(
            !report.errors[0].message.contains("is not one of"),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn an_unknown_key_in_a_named_entry_is_a_warning() {
        let report = validate(
            "permissions:\n  dev:\n    users: { root: {} }\n    colour: blue\nkernel:\n  lts: { package: k, colour: blue }\n",
        );
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(
            report.warnings,
            [
                "unknown key 'permissions.dev.colour' is ignored",
                "unknown key 'kernel.lts.colour' is ignored",
            ]
        );
    }
}

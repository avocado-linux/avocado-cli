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
    // `{{ }}` template in a boolean or number field is not a type error.
    let mut interpolated = doc.clone();
    let checked =
        match crate::utils::interpolation::interpolate_config(&mut interpolated, None, None) {
            Ok(()) => &interpolated,
            Err(_) => &doc,
        };
    match serde_json::to_value(checked) {
        Ok(instance) => {
            for e in validator().iter_errors(&instance) {
                report.errors.push(ConfigError {
                    path: e.instance_path().to_string(),
                    message: e.to_string(),
                });
            }
        }
        Err(e) => report.errors.push(ConfigError {
            path: String::new(),
            message: format!("not representable as JSON: {e}"),
        }),
    }

    // The loader's checks: shape inference, cross-references, and the rules
    // the schema cannot express. It stops at the first one.
    if let Err(e) = Config::load_from_yaml_str(content) {
        let message = e.root_cause().to_string();
        if !report.errors.iter().any(|r| r.message == message) {
            report.errors.push(ConfigError {
                path: String::new(),
                message,
            });
        }
    }
    report
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

/// The schema validator. `additionalProperties: false` is removed, because
/// the CLI ignores an unknown key and warns about it (`config_lint`); it is
/// not an error. Where an unknown key picks between one block and named
/// entries, the loader reports the mix.
fn validator() -> &'static jsonschema::Validator {
    static VALIDATOR: OnceLock<jsonschema::Validator> = OnceLock::new();
    VALIDATOR.get_or_init(|| {
        let mut schema = config_lint::schema().clone();
        relax(&mut schema);
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
    fn a_mixed_block_is_the_loader_error() {
        let errs = errors("kernel: {cmdline: a, other: {cmdline: b}}\n");
        assert!(
            errs.iter().any(|e| e.contains("cannot mix singleton form")),
            "{errs:?}"
        );
    }

    #[test]
    fn a_template_is_checked_after_interpolation() {
        let report = validate(
            "default_target: qemux86-64\nhw: tpm2\nruntimes:\n  dev:\n    var:\n      encrypt: true\n      hardware: \"{{ config.hw }}\"\n",
        );
        assert!(report.errors.is_empty(), "{:?}", report.errors);
    }
}

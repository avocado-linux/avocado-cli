//! `avocado vm config` — read/write the persistent VM config at
//! `~/.avocado/vm/config.yaml`. The file is shared with avocado-desktop;
//! anything desktop exposes should round-trip through these commands.
//!
//! Subcommands:
//!   - `get <key>`              print the value (YAML or JSON)
//!   - `set <key> <value...>`   write one (scalar) or many (list) values
//!   - `unset <key>`            remove a key
//!   - `list`                   print the whole config (YAML or JSON)

use anyhow::{Context, Result};

use crate::utils::output_format::{emit_json_object, OutputFormat};
use crate::utils::vm::config::VmConfig;
use crate::utils::vm::state::VmPaths;

pub struct ConfigGetCommand {
    pub key: String,
    pub output: OutputFormat,
}

impl ConfigGetCommand {
    pub async fn execute(self) -> Result<()> {
        let paths = VmPaths::resolve()?;
        let cfg = VmConfig::load(&paths)?;
        let value = cfg.get(&self.key)?;
        match (value, self.output) {
            (None, OutputFormat::Json) => {
                emit_json_object(&serde_json::json!({ "key": self.key, "value": null }));
            }
            (None, OutputFormat::Human) => {
                // Stay silent on stdout so `vm config get` is grep-safe in
                // scripts. Exit code stays 0 — missing != error.
            }
            (Some(v), OutputFormat::Json) => {
                let json_v: serde_json::Value =
                    serde_json::to_value(&v).context("yaml→json convert")?;
                emit_json_object(&serde_json::json!({ "key": self.key, "value": json_v }));
            }
            (Some(v), OutputFormat::Human) => print_yaml_value(&v),
        }
        Ok(())
    }
}

pub struct ConfigSetCommand {
    pub key: String,
    pub values: Vec<String>,
}

impl ConfigSetCommand {
    pub async fn execute(self) -> Result<()> {
        let paths = VmPaths::resolve()?;
        let mut cfg = VmConfig::load(&paths)?;
        cfg.set(&self.key, &self.values)?;
        cfg.save(&paths)?;
        Ok(())
    }
}

pub struct ConfigUnsetCommand {
    pub key: String,
}

impl ConfigUnsetCommand {
    pub async fn execute(self) -> Result<()> {
        let paths = VmPaths::resolve()?;
        let mut cfg = VmConfig::load(&paths)?;
        cfg.unset(&self.key)?;
        cfg.save(&paths)?;
        Ok(())
    }
}

pub struct ConfigListCommand {
    pub output: OutputFormat,
}

impl ConfigListCommand {
    pub async fn execute(self) -> Result<()> {
        let paths = VmPaths::resolve()?;
        let cfg = VmConfig::load(&paths)?;
        match self.output {
            OutputFormat::Json => {
                let v: serde_json::Value =
                    serde_json::to_value(&cfg).context("yaml→json convert")?;
                emit_json_object(&v);
            }
            OutputFormat::Human => {
                let yaml = serde_yaml::to_string(&cfg).context("serialize config")?;
                // Default-constructed config serializes to "{}\n" which is
                // confusing to read; print a friendlier hint instead.
                let trimmed = yaml.trim();
                if trimmed.is_empty() || trimmed == "{}" {
                    println!("(no config set; use `avocado vm config set <key> <value>`)");
                } else {
                    print!("{yaml}");
                }
            }
        }
        Ok(())
    }
}

fn print_yaml_value(v: &serde_yaml::Value) {
    // Render scalars as bare values (so callers can `vm config get ... | xargs`)
    // and structured values as YAML.
    match v {
        serde_yaml::Value::String(s) => println!("{s}"),
        serde_yaml::Value::Number(n) => println!("{n}"),
        serde_yaml::Value::Bool(b) => println!("{b}"),
        serde_yaml::Value::Null => {}
        other => {
            let s = serde_yaml::to_string(other).unwrap_or_default();
            print!("{s}");
        }
    }
}

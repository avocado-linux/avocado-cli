//! Centralized parsing for runtime extension list entries.
//!
//! A `runtimes.<name>.extensions:` list accepts two shapes:
//!
//! 1. Plain string — just the extension name; all options take defaults.
//!
//! ```yaml
//! extensions:
//!   - microclaw
//!   - vm-agent
//! ```
//!
//! 2. Single-key map — the key is the extension name, the value is a
//!    sub-mapping carrying per-extension knobs:
//!
//! ```yaml
//! extensions:
//!   - microclaw: { enabled: false }
//!   - avocado-ext-experimental:
//!       enabled: false
//! ```
//!
//! Every iterator that walks `runtime.extensions` must funnel through
//! [`parse_entry`] so a missed call site can never silently drop the new
//! map form (`as_str()` returns `None` on a map → the extension
//! disappears from `ext_deps`, `active_extensions`, etc., with no error).
//! Centralizing the parse also gives one place to grow new per-extension
//! flags (`merge_index:`, `runs_on:`, …) without touching the iterators.

use serde_yaml::Value;

/// Parsed view of one entry in a runtime's `extensions:` list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeExtensionSpec {
    /// The extension name (possibly still containing `{{ avocado.target }}`
    /// template placeholders — interpolation happens elsewhere).
    pub name: String,
    /// Whether this extension should be auto-activated when the runtime
    /// is provisioned. `false` ships the image but requires
    /// `avocadoctl ext enable <name>` to flip it on.
    pub enabled: bool,
}

impl RuntimeExtensionSpec {
    /// Parse a single list entry. Returns `None` for malformed shapes
    /// (multi-key map, empty list, non-string key, …) — the caller treats
    /// `None` the same way it used to treat a non-string entry: skip it.
    pub fn parse_entry(value: &Value) -> Option<Self> {
        // Plain string: `- microclaw`
        if let Some(s) = value.as_str() {
            return Some(Self {
                name: s.to_string(),
                enabled: true,
            });
        }
        // Single-key map: `- microclaw: { enabled: false }`
        let mapping = value.as_mapping()?;
        if mapping.len() != 1 {
            return None;
        }
        let (key, opts) = mapping.iter().next()?;
        let name = key.as_str()?.to_string();
        let enabled = parse_enabled(opts).unwrap_or(true);
        Some(Self { name, enabled })
    }
}

/// Extract `enabled` from the options sub-mapping. A null value
/// (`microclaw:`) is treated as "no options provided" → defaults to
/// enabled. Anything other than a boolean is ignored.
fn parse_enabled(opts: &Value) -> Option<bool> {
    if opts.is_null() {
        return None;
    }
    opts.as_mapping()?.get("enabled").and_then(|v| v.as_bool())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Vec<RuntimeExtensionSpec> {
        let v: Value = serde_yaml::from_str(yaml).unwrap();
        v.as_sequence()
            .unwrap()
            .iter()
            .filter_map(RuntimeExtensionSpec::parse_entry)
            .collect()
    }

    #[test]
    fn plain_string_entry() {
        let r = parse("- microclaw\n");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].name, "microclaw");
        assert!(r[0].enabled);
    }

    #[test]
    fn map_with_enabled_false() {
        let r = parse("- microclaw: { enabled: false }\n");
        assert_eq!(r[0].name, "microclaw");
        assert!(!r[0].enabled);
    }

    #[test]
    fn map_block_form() {
        let r = parse("- microclaw:\n    enabled: false\n");
        assert_eq!(r[0].name, "microclaw");
        assert!(!r[0].enabled);
    }

    #[test]
    fn map_without_options_value_is_enabled() {
        // `- microclaw:` parses as `{microclaw: null}` — treat as default.
        let r = parse("- microclaw:\n");
        assert_eq!(r[0].name, "microclaw");
        assert!(r[0].enabled);
    }

    #[test]
    fn templated_name_works_as_string_and_key() {
        let r = parse("- \"avocado-bsp-{{ avocado.target }}\"\n");
        assert_eq!(r[0].name, "avocado-bsp-{{ avocado.target }}");
        let r = parse("- \"avocado-bsp-{{ avocado.target }}\": { enabled: false }\n");
        assert_eq!(r[0].name, "avocado-bsp-{{ avocado.target }}");
        assert!(!r[0].enabled);
    }

    #[test]
    fn mixed_entries() {
        let r = parse(
            "- a\n\
             - b: { enabled: false }\n\
             - c\n",
        );
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].name, "a");
        assert!(r[0].enabled);
        assert_eq!(r[1].name, "b");
        assert!(!r[1].enabled);
        assert_eq!(r[2].name, "c");
        assert!(r[2].enabled);
    }

    #[test]
    fn multi_key_map_is_rejected() {
        let v: Value =
            serde_yaml::from_str("{ a: { enabled: false }, b: { enabled: true } }").unwrap();
        // Wrapped in a one-element list to feed parse_entry directly.
        let entry: Value = serde_yaml::to_value(&v).unwrap();
        assert!(RuntimeExtensionSpec::parse_entry(&entry).is_none());
    }
}

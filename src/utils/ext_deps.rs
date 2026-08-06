//! Inter-extension dependencies — declaration, closure expansion, ordering.
//!
//! An extension can declare that it is only meaningful when another extension
//! is merged alongside it. The canonical spelling is `depends_on`:
//!
//! ```yaml
//! extensions:
//!   weston-base:              # the shared platform base
//!     version: "1.2.0"
//!     class: platform
//!     packages: { weston: '*', wayland: '*' }
//!
//!   app-a:                    # binds to it
//!     version: "0.4.0"
//!     depends_on:
//!       - weston-base                            # any version
//!       - { name: gfx-core, version: ">=1.2.0" } # constrained
//! ```
//!
//! A distinct verb — not overloading the word `extensions`, not buried under
//! `packages`, and not colliding with RPM's own `Requires:` — so the
//! extension-to-extension edge is unmistakable at a glance.
//!
//! # Where resolution happens
//!
//! Resolution is a **build-side** concern. The author lists *intent* (just the
//! extensions they care about); the build expands the `depends_on` closure and
//! seals the complete ordered set into the runtime manifest. The device never
//! runs a solver — it only verifies and merges what the manifest already spells
//! out. Everything in this module therefore runs on the host, and every failure
//! it can produce is a build-time failure.
//!
//! # Two orderings, deliberately opposite
//!
//! - [`DependencyGraph::resolve`] orders **dependencies first**. This is an
//!   *install* order: a dependency's sysroot must exist before anything can be
//!   seeded from it.
//! - [`DependencyGraph::resolve_runtime_list`] orders **parents first**, each
//!   followed by what it pulls in. This is a *merge priority* order: avocadoctl
//!   treats earlier entries as higher priority, so an application must precede
//!   the platform base it depends on in order to win a file conflict against
//!   it.
//!
//! Using one for the other silently inverts merge precedence, so they are named
//! for their job rather than for their shape.
//!
//! # What this module does *not* do
//!
//! It resolves names and order only. Seeding an extension's rpmdb from its
//! dependencies (the mechanism that makes shared packages ship exactly once),
//! comparing version constraints against what a dependency actually shipped,
//! and content-pinning the sealed edges are separate, later stages that
//! consume the [`ResolvedClosure`] produced here.

// Resolve-only stage: the graph, its errors, and its lints are complete and
// tested, but no command calls them yet. The call sites (`ext install`
// ordering, `runtime build` closure sealing, `ext tree`) land in the following
// stages; drop this allow once the first one is wired up.
#![allow(dead_code)]

use anyhow::{bail, Context, Result};
use semver::VersionReq;
use serde_yaml::Value;
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::utils::config::ComposedConfig;
use crate::utils::interpolation::interpolate_name;

/// The role an extension plays in the dependency graph.
///
/// `depends_on` targets are *expected* to be `platform` — shared, ABI-stable
/// bases (a weston base, the `avocado-bsp-<board>` layer) released on a slower
/// cadence than the applications binding to them. An application depending on
/// another application is a smell (the shared part wants factoring out), so it
/// draws a lint warning rather than a hard error: it is a convention that keeps
/// the common case legible, not a gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExtensionClass {
    /// Default. A leaf extension that delivers a feature.
    #[default]
    Application,
    /// A shared, ABI-stable base that other extensions depend on.
    Platform,
}

impl ExtensionClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Application => "application",
            Self::Platform => "platform",
        }
    }

    /// Read `class:` off an extension's config block. Absent → `application`.
    ///
    /// An unrecognized value is an error, not a silent fall back to the
    /// default: a typo'd `class: platfrom` that quietly became an application
    /// would disable the very lint the key exists to drive.
    pub fn from_ext_config(ext_name: &str, ext: &Value) -> Result<Self> {
        let Some(raw) = ext.get("class") else {
            return Ok(Self::default());
        };
        let as_str = raw.as_str().ok_or_else(|| {
            anyhow::anyhow!(
                "Extension '{ext_name}': `class` must be a string, one of \
                 'application' or 'platform'."
            )
        })?;
        match as_str {
            "application" => Ok(Self::Application),
            "platform" => Ok(Self::Platform),
            other => bail!(
                "Extension '{ext_name}': unknown `class` value '{other}'. \
                 Expected 'application' (the default) or 'platform'."
            ),
        }
    }
}

/// One entry in an extension's `depends_on:` list.
///
/// Three spellings are accepted, all unambiguous:
///
/// | YAML                                        | Meaning                     |
/// |---------------------------------------------|-----------------------------|
/// | `- weston-base`                             | any version                 |
/// | `- { name: weston-base, version: ">=1.2" }` | explicit form               |
/// | `- { weston-base: ">=1.2" }`                | single-key shorthand        |
///
/// The explicit form is recognized by the presence of a `name` key, which is
/// also the escape hatch for an extension literally named `name`.
///
/// `version` is a standard semver *requirement* (`>=1.2.0`, `^1.2`, `*`,
/// `>=1.2, <2`), the same syntax `cli_requirement` uses. Note that a bare
/// `1.2.0` therefore means `^1.2.0`, not an exact pin — write `=1.2.0` for
/// that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionDependency {
    /// Target extension name, already interpolated (no `{{ … }}` left).
    pub name: String,
    /// Semver requirement, or `None` for "any version".
    pub version: Option<String>,
}

impl ExtensionDependency {
    /// Parse one `depends_on:` entry. `target` interpolates template names
    /// such as `avocado-bsp-{{ avocado.target }}`.
    pub fn parse_entry(ext_name: &str, value: &Value, target: &str) -> Result<Self> {
        // `- weston-base`
        if let Some(s) = value.as_str() {
            return Ok(Self {
                name: interpolate_name(s, target),
                version: None,
            });
        }

        let mapping = value.as_mapping().ok_or_else(|| {
            anyhow::anyhow!(
                "Extension '{ext_name}': each `depends_on` entry must be an extension \
                 name or a mapping, got {}.",
                describe(value)
            )
        })?;

        // `- { name: weston-base, version: ">=1.2.0" }`
        if let Some(name_val) = mapping.get("name") {
            let name = name_val.as_str().ok_or_else(|| {
                anyhow::anyhow!(
                    "Extension '{ext_name}': `depends_on` entry has a non-string `name`."
                )
            })?;
            let version = match mapping.get("version") {
                Some(v) => Some(require_version_string(ext_name, name, v)?),
                None => None,
            };
            return Self::new(ext_name, interpolate_name(name, target), version);
        }

        // `- { weston-base: ">=1.2.0" }` / `- { weston-base: { version: … } }`
        if mapping.len() != 1 {
            bail!(
                "Extension '{ext_name}': ambiguous `depends_on` entry with {} keys. \
                 Use `- {{ name: <ext>, version: <req> }}` for the explicit form.",
                mapping.len()
            );
        }
        let (key, opts) = mapping.iter().next().expect("len checked above");
        let name = key.as_str().ok_or_else(|| {
            anyhow::anyhow!("Extension '{ext_name}': `depends_on` entry has a non-string key.")
        })?;
        let version = if opts.is_null() {
            None
        } else if let Some(sub) = opts.as_mapping() {
            match sub.get("version") {
                Some(v) => Some(require_version_string(ext_name, name, v)?),
                None => None,
            }
        } else {
            Some(require_version_string(ext_name, name, opts)?)
        };
        Self::new(ext_name, interpolate_name(name, target), version)
    }

    fn new(ext_name: &str, name: String, version: Option<String>) -> Result<Self> {
        if name.trim().is_empty() {
            bail!("Extension '{ext_name}': `depends_on` entry has an empty extension name.");
        }
        let dep = Self { name, version };
        // Validate the constraint here so a typo fails at parse with the
        // offending extension named, rather than deep inside the resolver.
        dep.version_req()?;
        Ok(dep)
    }

    /// The parsed semver requirement, or `None` for "any version".
    pub fn version_req(&self) -> Result<Option<VersionReq>> {
        self.version
            .as_deref()
            .map(|raw| {
                VersionReq::parse(raw).with_context(|| {
                    format!(
                        "Invalid version requirement '{raw}' on dependency '{}'. \
                         Expected semver requirement syntax (e.g. '>=1.2.0', '^1.2', '*').",
                        self.name
                    )
                })
            })
            .transpose()
    }
}

/// The RPM virtual capability an extension provides, and that its dependents
/// require.
///
/// Indirection through a virtual provide rather than the bare RPM name is
/// deliberate: `source.package` lets a consumer publish an extension under a
/// different RPM name, which would break a literal `Requires: <ext-name>`.
/// The provide is stable regardless of what the package is called.
pub fn rpm_capability(ext_name: &str) -> String {
    format!("avocado-ext({ext_name})")
}

/// Translate a semver requirement into RPM version comparisons.
///
/// Returns `(operator, version)` pairs that must *all* hold — RPM expresses a
/// range as several `Requires:` lines on the same capability, so `^1.2.3`
/// becomes `>= 1.2.3` plus `< 2.0.0`. An unconstrained requirement (`*`)
/// returns empty, meaning a bare unversioned `Requires:`.
///
/// Semver's caret/tilde/wildcard have no RPM equivalent, so they are expanded
/// into explicit bounds here rather than approximated at the call site.
pub fn rpm_version_constraints(req: &VersionReq) -> Vec<(&'static str, String)> {
    use semver::Op;

    let mut out = Vec::new();
    for c in &req.comparators {
        let minor = c.minor.unwrap_or(0);
        let patch = c.patch.unwrap_or(0);
        let at = |ma: u64, mi: u64, pa: u64| format!("{ma}.{mi}.{pa}");
        let lower = if c.pre.is_empty() {
            at(c.major, minor, patch)
        } else {
            format!("{}.{}.{}~{}", c.major, minor, patch, c.pre)
        };

        match c.op {
            Op::Exact => out.push(("=", lower)),
            Op::Greater => out.push((">", lower)),
            Op::GreaterEq => out.push((">=", lower)),
            Op::Less => out.push(("<", lower)),
            Op::LessEq => out.push(("<=", lower)),
            // ^1.2.3 → >=1.2.3, <2.0.0 | ^0.2.3 → >=0.2.3, <0.3.0
            // ^0.0.3 → >=0.0.3, <0.0.4 — leading zeros narrow the range.
            Op::Caret => {
                out.push((">=", lower));
                let upper = if c.major > 0 {
                    at(c.major + 1, 0, 0)
                } else if c.minor.is_none() {
                    at(1, 0, 0)
                } else if minor > 0 || c.patch.is_none() {
                    at(0, minor + 1, 0)
                } else {
                    at(0, 0, patch + 1)
                };
                out.push(("<", upper));
            }
            // ~1.2.3 and ~1.2 → >=…, <1.3.0 | ~1 → >=1.0.0, <2.0.0
            Op::Tilde => {
                out.push((">=", lower));
                let upper = if c.minor.is_some() {
                    at(c.major, minor + 1, 0)
                } else {
                    at(c.major + 1, 0, 0)
                };
                out.push(("<", upper));
            }
            // `1.*` → >=1.0.0, <2.0.0 | `1.2.*` → >=1.2.0, <1.3.0 | `*` → none
            Op::Wildcard => {
                if c.minor.is_some() {
                    out.push((">=", lower));
                    out.push(("<", at(c.major, minor + 1, 0)));
                } else {
                    out.push((">=", at(c.major, 0, 0)));
                    out.push(("<", at(c.major + 1, 0, 0)));
                }
            }
            // semver is non_exhaustive; an unknown operator is better dropped
            // than mistranslated into a wrong bound.
            _ => {}
        }
    }
    out
}

impl ExtensionDependency {
    /// The `Requires:` lines this edge contributes to a generated spec.
    ///
    /// Multiple lines for a range; one bare line when unconstrained.
    pub fn to_rpm_requires(&self) -> Result<Vec<String>> {
        let cap = rpm_capability(&self.name);
        let Some(req) = self.version_req()? else {
            return Ok(vec![format!("Requires: {cap}")]);
        };
        let constraints = rpm_version_constraints(&req);
        if constraints.is_empty() {
            return Ok(vec![format!("Requires: {cap}")]);
        }
        Ok(constraints
            .into_iter()
            .map(|(op, v)| format!("Requires: {cap} {op} {v}"))
            .collect())
    }
}

/// Whether moving from `was` to `now` is a downgrade, by rpm version ordering.
///
/// Used to distinguish a dependency being pulled *forward* (routine — the lock
/// records a solution, and a changed constraint means a new solution) from one
/// being dragged *backward*, which has no benign reading: it means something
/// old is constraining a shared base below what is already deployed.
///
/// Implements rpmvercmp's segment rule rather than semver: these are RPM
/// `VERSION-RELEASE` strings, where `1.10 > 1.9` and a numeric segment always
/// outranks an alphabetic one. A `~` segment sorts *before* everything, which
/// is how pre-releases order.
pub fn is_rpm_downgrade(was: &str, now: &str) -> bool {
    rpm_vercmp(was, now) == std::cmp::Ordering::Greater
}

/// Compare two RPM version strings, rpmvercmp-style.
fn rpm_vercmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let mut a = a.chars().peekable();
    let mut b = b.chars().peekable();

    loop {
        // `~` sorts before everything, including the empty string.
        let a_tilde = a.peek() == Some(&'~');
        let b_tilde = b.peek() == Some(&'~');
        if a_tilde || b_tilde {
            match (a_tilde, b_tilde) {
                (true, true) => {
                    a.next();
                    b.next();
                    continue;
                }
                (true, false) => return Ordering::Less,
                (false, true) => return Ordering::Greater,
                _ => unreachable!(),
            }
        }

        // Skip separators.
        while a.peek().is_some_and(|c| !c.is_alphanumeric() && *c != '~') {
            a.next();
        }
        while b.peek().is_some_and(|c| !c.is_alphanumeric() && *c != '~') {
            b.next();
        }

        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) => {
                let a_num = ca.is_ascii_digit();
                let b_num = cb.is_ascii_digit();
                // A numeric segment always outranks an alphabetic one.
                if a_num != b_num {
                    return if a_num {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    };
                }

                let take = |it: &mut std::iter::Peekable<std::str::Chars>, numeric: bool| {
                    let mut s = String::new();
                    while let Some(&c) = it.peek() {
                        if numeric && c.is_ascii_digit() || !numeric && c.is_alphabetic() {
                            s.push(c);
                            it.next();
                        } else {
                            break;
                        }
                    }
                    s
                };
                let sa = take(&mut a, a_num);
                let sb = take(&mut b, b_num);

                let ord = if a_num {
                    // Strip leading zeros, then longer digit run wins.
                    let ta = sa.trim_start_matches('0');
                    let tb = sb.trim_start_matches('0');
                    ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb))
                } else {
                    sa.cmp(&sb)
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

/// Extract just the extension *names* from a `depends_on:` list, ignoring
/// version constraints and tolerating malformed entries.
///
/// Deliberately lenient, and deliberately separate from
/// [`ExtensionDependency::parse_entry`]: this runs during config composition,
/// before the graph is built, to answer the narrow question "which sibling
/// extension declarations do I need to pull in?". Composition is not the right
/// place to reject a malformed `depends_on` — that diagnostic belongs to
/// [`DependencyGraph::from_extensions_section`], which sees the whole picture
/// and can name the offending extension. An entry this skips simply fails
/// later, with a better message.
pub fn dependency_names(ext_value: &Value) -> Vec<String> {
    let Some(seq) = ext_value.get("depends_on").and_then(|d| d.as_sequence()) else {
        return Vec::new();
    };
    seq.iter()
        .filter_map(|entry| {
            if let Some(s) = entry.as_str() {
                return Some(s.to_string());
            }
            let mapping = entry.as_mapping()?;
            if let Some(name) = mapping.get("name").and_then(|v| v.as_str()) {
                return Some(name.to_string());
            }
            if mapping.len() == 1 {
                return mapping.iter().next()?.0.as_str().map(str::to_string);
            }
            None
        })
        .collect()
}

fn require_version_string(ext_name: &str, dep_name: &str, v: &Value) -> Result<String> {
    v.as_str().map(str::to_string).ok_or_else(|| {
        anyhow::anyhow!(
            "Extension '{ext_name}': `depends_on` entry for '{dep_name}' has a \
             non-string `version`. Quote it (e.g. version: \">=1.2.0\")."
        )
    })
}

fn describe(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Sequence(_) => "a list",
        Value::Mapping(_) => "a mapping",
        Value::Tagged(_) => "a tagged value",
    }
}

/// Where an extension's definition came from — and, crucially, whether it
/// arrived at all.
///
/// An extension reaches the config in one of several ways, and they differ in
/// *when* the extension's own `avocado.yaml` (the file that carries its
/// `depends_on`) becomes readable:
///
/// - **inline** — written directly in the consumer's `avocado.yaml`. Always
///   available.
/// - **`source: { type: path }`** — read straight off the host path at compose
///   time. Available without any fetch.
/// - **`source: { type: git | package }`** — only readable after the extension
///   has been fetched into `$AVOCADO_PREFIX/<target>/includes/<ext>/`. Before
///   that, `Config::load_composed` finds nothing to merge and the extension is
///   left as a bare `source:` stub.
///
/// That last case is the dangerous one: a stub has no `depends_on` key, which
/// is indistinguishable *by shape* from an extension that genuinely has no
/// dependencies. Resolving it as dependency-free would silently drop its whole
/// subtree from the closure and ship an image missing its platform base. So
/// the graph tracks it explicitly and [`DependencyGraph::resolve`] refuses to
/// walk through it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionOrigin {
    /// Definition was read — inline, or a remote whose config was merged.
    Defined,
    /// Declared via `source:`, but its config was never merged. The string is
    /// the declared source type (`git`, `package`, `path`, …) for diagnostics.
    Unfetched(String),
}

/// One extension as the dependency graph sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionNode {
    /// Interpolated extension name — the graph's key.
    pub name: String,
    pub class: ExtensionClass,
    /// The extension's own `version:`, if it declares one.
    pub version: Option<String>,
    /// Outgoing edges, in declaration order. Empty and meaningless when
    /// `origin` is [`ExtensionOrigin::Unfetched`].
    pub depends_on: Vec<ExtensionDependency>,
    pub origin: ExtensionOrigin,
}

/// The `extensions:` section viewed as a dependency graph.
///
/// Build it from the **composed** merged config, never the raw on-disk
/// consumer yaml: a remote extension carries only a `source:` block in the
/// consumer's file, and its real fields — including its `depends_on` — are
/// merged in by `Config::load_composed`.
#[derive(Debug, Clone, Default)]
pub struct DependencyGraph {
    /// Keyed by interpolated name. `BTreeMap` so any full-graph iteration
    /// (lints, diagnostics) is deterministic.
    nodes: BTreeMap<String, ExtensionNode>,
}

impl DependencyGraph {
    /// Build the graph from a composed config — the normal entry point.
    ///
    /// Uses `ComposedConfig::extension_sources` to tell a merged remote
    /// extension from an unfetched stub: `load_composed` records every
    /// extension against the main config first, then *overwrites* the entry
    /// with the extension's own config path once it successfully merges that
    /// extension's `avocado.yaml`. An extension that declares a `source:` but
    /// still points at the main config was therefore never fetched.
    pub fn from_composed(composed: &ComposedConfig, target: &str) -> Result<Self> {
        let Some(extensions) = composed.merged_value.get("extensions") else {
            return Ok(Self::default());
        };

        let mut unfetched = HashSet::new();
        if let Some(mapping) = extensions.as_mapping() {
            for (key, ext) in mapping {
                let Some(raw_name) = key.as_str() else {
                    continue;
                };
                if ext.get("source").is_none() {
                    continue;
                }
                let merged_from = composed
                    .extension_sources
                    .get(raw_name)
                    .or_else(|| {
                        composed
                            .extension_sources
                            .get(&interpolate_name(raw_name, target))
                    })
                    .map(String::as_str);
                if merged_from.is_none() || merged_from == Some(composed.config_path.as_str()) {
                    unfetched.insert(interpolate_name(raw_name, target));
                }
            }
        }

        Self::from_extensions_section(extensions, target, &unfetched)
    }

    /// Build the graph from an `extensions:` mapping, given the set of
    /// extension names known to be declared-but-unfetched.
    ///
    /// Prefer [`Self::from_composed`], which derives that set for you. This
    /// form exists for callers that already know their content is complete
    /// (and for tests); passing an empty set asserts exactly that.
    ///
    /// Every key and every `depends_on` name is interpolated against `target`
    /// up front, so the graph holds concrete names and all downstream matching
    /// is plain string equality — no template-aware lookup at each call site.
    pub fn from_extensions_section(
        extensions: &Value,
        target: &str,
        unfetched: &HashSet<String>,
    ) -> Result<Self> {
        let mut nodes = BTreeMap::new();

        let Some(mapping) = extensions.as_mapping() else {
            // No `extensions:` section, or a malformed one — an empty graph.
            // Callers that need a specific extension get a precise
            // "not defined" error from `resolve`.
            return Ok(Self { nodes });
        };

        for (key, ext) in mapping {
            let Some(raw_name) = key.as_str() else {
                continue;
            };
            let name = interpolate_name(raw_name, target);

            let class = ExtensionClass::from_ext_config(&name, ext)?;
            let version = ext
                .get("version")
                .and_then(|v| v.as_str())
                .map(str::to_string);

            let origin = if unfetched.contains(&name) {
                let source_type = ext
                    .get("source")
                    .and_then(|s| s.get("type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("remote");
                ExtensionOrigin::Unfetched(source_type.to_string())
            } else {
                ExtensionOrigin::Defined
            };

            let mut depends_on = Vec::new();
            if let Some(list) = ext.get("depends_on") {
                let seq = list.as_sequence().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Extension '{name}': `depends_on` must be a list, got {}.",
                        describe(list)
                    )
                })?;
                for entry in seq {
                    let dep = ExtensionDependency::parse_entry(&name, entry, target)?;
                    if dep.name == name {
                        bail!("Extension '{name}' declares a `depends_on` edge to itself.");
                    }
                    depends_on.push(dep);
                }
            }

            nodes.insert(
                name.clone(),
                ExtensionNode {
                    name,
                    class,
                    version,
                    depends_on,
                    origin,
                },
            );
        }

        Ok(Self { nodes })
    }

    pub fn get(&self, name: &str) -> Option<&ExtensionNode> {
        self.nodes.get(name)
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Expand `roots` to their transitive `depends_on` closure and return it
    /// topologically ordered, **dependencies before dependents**.
    ///
    /// Ordering is a depth-first post-order over roots in author order, so it
    /// is fully deterministic: a dependency lands as early as its own
    /// dependencies allow, and otherwise ties break by the order the author
    /// wrote things. Under de-duplication the extensions' files are disjoint,
    /// so order is not load-bearing for merge correctness — but a stable order
    /// keeps the sealed manifest reproducible and diffable, and it is exactly
    /// the order in which extensions must be *installed* for each one's
    /// sysroot to exist before its dependents are seeded from it.
    ///
    /// Errors, both naming the chain that led there:
    /// - a dependency that is not defined in `extensions:`,
    /// - a cycle (reported with the full path, not silently truncated).
    pub fn resolve(&self, roots: &[String]) -> Result<ResolvedClosure> {
        let mut unique_roots = Vec::new();
        let mut seen = HashSet::new();
        for root in roots {
            if seen.insert(root.as_str()) {
                unique_roots.push(root.clone());
            }
        }

        let mut marks: HashMap<&str, Mark> = HashMap::new();
        let mut chain: Vec<&str> = Vec::new();
        let mut order: Vec<String> = Vec::new();

        for root in &unique_roots {
            self.visit(root, &mut marks, &mut chain, &mut order)?;
        }

        Ok(ResolvedClosure {
            order,
            roots: unique_roots,
        })
    }

    fn visit<'a>(
        &'a self,
        name: &'a str,
        marks: &mut HashMap<&'a str, Mark>,
        chain: &mut Vec<&'a str>,
        order: &mut Vec<String>,
    ) -> Result<()> {
        match marks.get(name) {
            Some(Mark::Done) => return Ok(()),
            // Back-edge onto an extension still open on the stack: a cycle.
            Some(Mark::InProgress) => {
                let start = chain.iter().position(|n| *n == name).unwrap_or(0);
                let mut path: Vec<&str> = chain[start..].to_vec();
                path.push(name);
                bail!(
                    "Dependency cycle between extensions: {}.\n\
                     Extensions cannot depend on each other in a loop — factor the \
                     shared part into a separate `class: platform` extension.",
                    path.join(" -> ")
                );
            }
            None => {}
        }

        let node = self.nodes.get(name).ok_or_else(|| {
            let chain_note = if chain.is_empty() {
                String::new()
            } else {
                format!("\nRequired by: {} -> {name}", chain.join(" -> "))
            };
            anyhow::anyhow!(
                "Extension '{name}' is not defined in `extensions:` and could not be \
                 resolved from the target's feed.{chain_note}"
            )
        })?;

        // A `git`/`package` extension that has not been fetched is a bare
        // `source:` stub: its `depends_on` is not merely empty, it is unknown.
        // Walking through it would silently drop its subtree from the closure
        // and produce an image missing its platform base, so stop here instead.
        if let ExtensionOrigin::Unfetched(source_type) = &node.origin {
            let chain_note = if chain.is_empty() {
                String::new()
            } else {
                format!("\nRequired by: {} -> {name}", chain.join(" -> "))
            };
            let remedy = if source_type == "path" {
                "Its `source: { type: path }` directory has no readable avocado.yaml \
                 — check the path is correct and the file exists."
            } else {
                "Run `avocado ext fetch` to fetch it before resolving dependencies."
            };
            bail!(
                "Extension '{name}' declares `source: {{ type: {source_type} }}` but its \
                 configuration has not been merged, so its dependencies are unknown.\n\
                 {remedy}{chain_note}"
            );
        }

        marks.insert(&node.name, Mark::InProgress);
        chain.push(&node.name);
        for dep in &node.depends_on {
            self.visit(&dep.name, marks, chain, order)?;
        }
        chain.pop();
        marks.insert(&node.name, Mark::Done);
        order.push(node.name.clone());

        Ok(())
    }

    /// Expand a runtime's authored extension list into the complete set to
    /// ship, ordered for **merge priority**.
    ///
    /// The author places only what they care about; each entry is followed by
    /// the dependencies it pulls in, flattened depth-first:
    ///
    /// ```text
    /// authored: [app-a, app-b]      ->  app-a        (authored)
    ///                                     base       (implied by app-a)
    ///                                   app-b        (authored)
    ///                                     mid        (implied by app-b)
    ///                                       base     (already placed, skipped)
    /// ```
    ///
    /// # Why parent-first, when installation is dependency-first
    ///
    /// These are deliberately opposite orderings for two different jobs.
    ///
    /// [`Self::resolve`] orders dependencies *first* because that is an
    /// **install** order: a dependency's sysroot has to exist before anything
    /// can be seeded from it.
    ///
    /// This list is a **merge priority** order. avocadoctl computes
    /// `merge_idx = ext_count - 1 - index`, so earlier means higher priority.
    /// Putting the parent first lets an application win a file conflict
    /// against the platform base it depends on — the specific thing beats the
    /// shared thing it was built on. Dependencies-first would invert that and
    /// let a base silently shadow its dependent.
    ///
    /// # The author's own placement is truth
    ///
    /// If the author lists an extension that would also have been implied,
    /// their entry wins: it is emitted once, at the position they chose, with
    /// the options they set. The expansion never re-places it. That keeps
    /// `- weston-base: {{ enabled: false }}` meaningful even though something
    /// else depends on it.
    ///
    /// The graph is validated by [`Self::resolve`] first, so cycles, missing
    /// dependencies, and unfetched stubs are all reported before any ordering
    /// happens.
    pub fn resolve_runtime_list(&self, authored: &[String]) -> Result<Vec<RuntimeEntry>> {
        // Validate the whole closure up front — reuses the cycle, missing-dep,
        // and unfetched-stub diagnostics rather than reimplementing them.
        self.resolve(authored)?;

        let authored_set: HashSet<&str> = authored.iter().map(String::as_str).collect();
        let mut emitted: HashSet<String> = HashSet::new();
        let mut out: Vec<RuntimeEntry> = Vec::new();

        for name in authored {
            if !emitted.insert(name.clone()) {
                // The author listed the same extension twice; keep the first.
                continue;
            }
            out.push(RuntimeEntry {
                name: name.clone(),
                authored: true,
            });
            self.push_implied(name, &authored_set, &mut emitted, &mut out);
        }

        Ok(out)
    }

    fn push_implied(
        &self,
        name: &str,
        authored_set: &HashSet<&str>,
        emitted: &mut HashSet<String>,
        out: &mut Vec<RuntimeEntry>,
    ) {
        let Some(node) = self.nodes.get(name) else {
            return;
        };
        for dep in &node.depends_on {
            // The author placed this one themselves — theirs is truth, and it
            // is emitted at their position rather than here.
            if authored_set.contains(dep.name.as_str()) {
                continue;
            }
            if !emitted.insert(dep.name.clone()) {
                continue;
            }
            out.push(RuntimeEntry {
                name: dep.name.clone(),
                authored: false,
            });
            // Safe to recurse: `resolve` already proved the graph acyclic.
            self.push_implied(&dep.name, authored_set, emitted, out);
        }
    }

    /// Advisory checks over a resolved closure. Warnings, never failures —
    /// these describe smells, not broken builds.
    pub fn lint(&self, closure: &ResolvedClosure) -> Vec<String> {
        let mut warnings = Vec::new();

        for name in &closure.order {
            let Some(node) = self.nodes.get(name) else {
                continue;
            };
            if node.class != ExtensionClass::Application {
                continue;
            }
            for dep in &node.depends_on {
                let Some(dep_node) = self.nodes.get(&dep.name) else {
                    continue;
                };
                if dep_node.class == ExtensionClass::Application {
                    warnings.push(format!(
                        "Extension '{}' (class: application) depends on '{}', which is also \
                         class: application. Shared bases are normally `class: platform` — \
                         consider factoring the shared part out, or mark '{}' as platform.",
                        node.name, dep_node.name, dep_node.name
                    ));
                }
            }
        }

        warnings
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    /// Open on the DFS stack — reaching it again is a back-edge.
    InProgress,
    /// Fully emitted into `order`.
    Done,
}

/// One entry in a runtime's resolved extension list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeEntry {
    pub name: String,
    /// `true` when the author placed it in `runtimes.<name>.extensions`,
    /// `false` when it was pulled in as a dependency. Drives `ext tree` and
    /// answers "why is this in my image?".
    pub authored: bool,
}

/// A `depends_on` closure, expanded and ordered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedClosure {
    /// The full closure, dependencies before dependents. Each extension
    /// appears exactly once.
    pub order: Vec<String>,
    /// The extensions that were asked for, deduplicated, in author order.
    pub roots: Vec<String>,
}

impl ResolvedClosure {
    /// Closure members that were pulled in as dependencies rather than
    /// requested by the author — what `avocado ext tree` highlights, and the
    /// answer to "why is this extension in my image?".
    pub fn implied(&self) -> Vec<&str> {
        self.order
            .iter()
            .map(String::as_str)
            .filter(|name| !self.roots.iter().any(|r| r == name))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(yaml: &str) -> DependencyGraph {
        graph_for(yaml, "qemux86-64")
    }

    fn graph_for(yaml: &str, target: &str) -> DependencyGraph {
        let v: Value = serde_yaml::from_str(yaml).unwrap();
        DependencyGraph::from_extensions_section(
            v.get("extensions").unwrap(),
            target,
            &HashSet::new(),
        )
        .unwrap()
    }

    /// Graph where `unfetched` names are declared-but-not-merged stubs.
    fn graph_unfetched(yaml: &str, unfetched: &[&str]) -> DependencyGraph {
        let v: Value = serde_yaml::from_str(yaml).unwrap();
        let set: HashSet<String> = unfetched.iter().map(|s| s.to_string()).collect();
        DependencyGraph::from_extensions_section(v.get("extensions").unwrap(), "qemux86-64", &set)
            .unwrap()
    }

    fn graph_err(yaml: &str) -> String {
        let v: Value = serde_yaml::from_str(yaml).unwrap();
        DependencyGraph::from_extensions_section(
            v.get("extensions").unwrap(),
            "qemux86-64",
            &HashSet::new(),
        )
        .unwrap_err()
        .to_string()
    }

    fn roots(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    // ---- declaration parsing -------------------------------------------

    #[test]
    fn plain_string_dependency() {
        let g = graph(
            "extensions:\n\
             \x20 weston-base: {}\n\
             \x20 app-a:\n\
             \x20   depends_on: [weston-base]\n",
        );
        let deps = &g.get("app-a").unwrap().depends_on;
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "weston-base");
        assert_eq!(deps[0].version, None);
    }

    #[test]
    fn explicit_name_version_form() {
        let g = graph(
            "extensions:\n\
             \x20 app-a:\n\
             \x20   depends_on:\n\
             \x20     - { name: weston-base, version: \">=1.2.0\" }\n",
        );
        let dep = &g.get("app-a").unwrap().depends_on[0];
        assert_eq!(dep.name, "weston-base");
        assert_eq!(dep.version.as_deref(), Some(">=1.2.0"));
        assert!(dep.version_req().unwrap().is_some());
    }

    #[test]
    fn single_key_shorthand_forms() {
        let g = graph(
            "extensions:\n\
             \x20 app-a:\n\
             \x20   depends_on:\n\
             \x20     - { weston-base: \">=1.2.0\" }\n\
             \x20     - { gfx-core: { version: \"^2\" } }\n\
             \x20     - { plain-base: }\n",
        );
        let deps = &g.get("app-a").unwrap().depends_on;
        assert_eq!(deps[0].name, "weston-base");
        assert_eq!(deps[0].version.as_deref(), Some(">=1.2.0"));
        assert_eq!(deps[1].name, "gfx-core");
        assert_eq!(deps[1].version.as_deref(), Some("^2"));
        assert_eq!(deps[2].name, "plain-base");
        assert_eq!(deps[2].version, None);
    }

    #[test]
    fn extension_literally_named_name_uses_explicit_form() {
        // `- { name: "1.0" }` is the explicit form with a bogus name, not a
        // shorthand for an extension called `name`. The explicit form is how
        // you actually depend on one.
        let g = graph(
            "extensions:\n\
             \x20 app-a:\n\
             \x20   depends_on:\n\
             \x20     - { name: name, version: \"^1\" }\n",
        );
        let dep = &g.get("app-a").unwrap().depends_on[0];
        assert_eq!(dep.name, "name");
        assert_eq!(dep.version.as_deref(), Some("^1"));
    }

    #[test]
    fn template_names_are_interpolated_on_both_sides() {
        let g = graph_for(
            "extensions:\n\
             \x20 \"avocado-bsp-{{ avocado.target }}\": { class: platform }\n\
             \x20 app-a:\n\
             \x20   depends_on: [\"avocado-bsp-{{ avocado.target }}\"]\n",
            "jetson-orin-nano-devkit",
        );
        assert!(g.get("avocado-bsp-jetson-orin-nano-devkit").is_some());
        assert_eq!(
            g.get("app-a").unwrap().depends_on[0].name,
            "avocado-bsp-jetson-orin-nano-devkit"
        );
        // …and it resolves as a normal edge.
        let c = g.resolve(&roots(&["app-a"])).unwrap();
        assert_eq!(
            c.order,
            vec!["avocado-bsp-jetson-orin-nano-devkit", "app-a"]
        );
    }

    #[test]
    fn invalid_version_requirement_is_rejected_at_parse() {
        let err = graph_err(
            "extensions:\n\
             \x20 app-a:\n\
             \x20   depends_on:\n\
             \x20     - { name: weston-base, version: \"not-a-version\" }\n",
        );
        assert!(err.contains("weston-base"), "{err}");
    }

    #[test]
    fn depends_on_must_be_a_list() {
        let err = graph_err(
            "extensions:\n\
             \x20 app-a:\n\
             \x20   depends_on: weston-base\n",
        );
        assert!(err.contains("must be a list"), "{err}");
    }

    #[test]
    fn self_edge_is_rejected() {
        let err = graph_err(
            "extensions:\n\
             \x20 app-a:\n\
             \x20   depends_on: [app-a]\n",
        );
        assert!(err.contains("itself"), "{err}");
    }

    // ---- class ----------------------------------------------------------

    #[test]
    fn class_defaults_to_application_and_parses_platform() {
        let g = graph(
            "extensions:\n\
             \x20 app-a: {}\n\
             \x20 weston-base: { class: platform }\n\
             \x20 app-b: { class: application }\n",
        );
        assert_eq!(g.get("app-a").unwrap().class, ExtensionClass::Application);
        assert_eq!(
            g.get("weston-base").unwrap().class,
            ExtensionClass::Platform
        );
        assert_eq!(g.get("app-b").unwrap().class, ExtensionClass::Application);
    }

    #[test]
    fn unknown_class_errors_rather_than_defaulting() {
        let err = graph_err("extensions:\n \x20weston-base: { class: platfrom }\n");
        assert!(err.contains("platfrom"), "{err}");
    }

    // ---- closure + ordering ---------------------------------------------

    #[test]
    fn dependency_is_ordered_before_its_dependent() {
        let g = graph(
            "extensions:\n\
             \x20 weston-base: { class: platform }\n\
             \x20 app-a:\n\
             \x20   depends_on: [weston-base]\n",
        );
        let c = g.resolve(&roots(&["app-a"])).unwrap();
        assert_eq!(c.order, vec!["weston-base", "app-a"]);
        assert_eq!(c.roots, vec!["app-a"]);
        assert_eq!(c.implied(), vec!["weston-base"]);
    }

    #[test]
    fn transitive_chain_is_fully_expanded() {
        let g = graph(
            "extensions:\n\
             \x20 base: { class: platform }\n\
             \x20 mid:\n\
             \x20   class: platform\n\
             \x20   depends_on: [base]\n\
             \x20 app-a:\n\
             \x20   depends_on: [mid]\n",
        );
        let c = g.resolve(&roots(&["app-a"])).unwrap();
        assert_eq!(c.order, vec!["base", "mid", "app-a"]);
    }

    #[test]
    fn diamond_ships_the_shared_base_once() {
        // D -> {B, C}, B -> A, C -> A
        let g = graph(
            "extensions:\n\
             \x20 a: { class: platform }\n\
             \x20 b: { class: platform, depends_on: [a] }\n\
             \x20 c: { class: platform, depends_on: [a] }\n\
             \x20 d: { depends_on: [b, c] }\n",
        );
        let c = g.resolve(&roots(&["d"])).unwrap();
        assert_eq!(c.order, vec!["a", "b", "c", "d"]);
        assert_eq!(c.order.iter().filter(|n| *n == "a").count(), 1);
    }

    #[test]
    fn independent_roots_keep_author_order() {
        let g = graph(
            "extensions:\n\
             \x20 weston-base: { class: platform }\n\
             \x20 app-a: { depends_on: [weston-base] }\n\
             \x20 app-b: { depends_on: [weston-base] }\n",
        );
        let c = g.resolve(&roots(&["app-a", "app-b"])).unwrap();
        assert_eq!(c.order, vec!["weston-base", "app-a", "app-b"]);

        // Reversing the author's list reverses only the tie, never an edge.
        let c = g.resolve(&roots(&["app-b", "app-a"])).unwrap();
        assert_eq!(c.order, vec!["weston-base", "app-b", "app-a"]);
    }

    #[test]
    fn author_may_also_list_a_platform_explicitly() {
        // Harmless and deduplicated — one entry, still ordered before its
        // dependent, and no longer reported as "implied".
        let g = graph(
            "extensions:\n\
             \x20 weston-base: { class: platform }\n\
             \x20 app-a: { depends_on: [weston-base] }\n",
        );
        let c = g.resolve(&roots(&["app-a", "weston-base"])).unwrap();
        assert_eq!(c.order, vec!["weston-base", "app-a"]);
        assert!(c.implied().is_empty());
    }

    #[test]
    fn duplicate_roots_are_deduplicated() {
        let g = graph("extensions:\n \x20app-a: {}\n");
        let c = g.resolve(&roots(&["app-a", "app-a"])).unwrap();
        assert_eq!(c.order, vec!["app-a"]);
        assert_eq!(c.roots, vec!["app-a"]);
    }

    // ---- failure modes ---------------------------------------------------

    #[test]
    fn cycle_is_reported_with_the_full_path() {
        let g = graph(
            "extensions:\n\
             \x20 a: { depends_on: [b] }\n\
             \x20 b: { depends_on: [c] }\n\
             \x20 c: { depends_on: [a] }\n",
        );
        let err = g.resolve(&roots(&["a"])).unwrap_err().to_string();
        assert!(err.contains("a -> b -> c -> a"), "{err}");
    }

    #[test]
    fn cycle_not_touching_the_root_still_reports_only_the_loop() {
        let g = graph(
            "extensions:\n\
             \x20 root: { depends_on: [b] }\n\
             \x20 b: { depends_on: [c] }\n\
             \x20 c: { depends_on: [b] }\n",
        );
        let err = g.resolve(&roots(&["root"])).unwrap_err().to_string();
        assert!(err.contains("b -> c -> b"), "{err}");
        assert!(!err.contains("root ->"), "{err}");
    }

    #[test]
    fn missing_dependency_names_the_chain() {
        let g = graph(
            "extensions:\n\
             \x20 app-a: { depends_on: [mid] }\n\
             \x20 mid: { depends_on: [nope] }\n",
        );
        let err = g.resolve(&roots(&["app-a"])).unwrap_err().to_string();
        assert!(err.contains("'nope' is not defined"), "{err}");
        assert!(err.contains("app-a -> mid -> nope"), "{err}");
    }

    #[test]
    fn missing_root_errors_without_a_chain_note() {
        let g = graph("extensions:\n \x20app-a: {}\n");
        let err = g.resolve(&roots(&["ghost"])).unwrap_err().to_string();
        assert!(err.contains("'ghost' is not defined"), "{err}");
        assert!(!err.contains("Required by"), "{err}");
    }

    // ---- rpm version ordering (downgrade detection) ----------------------

    #[test]
    fn forward_moves_are_not_downgrades() {
        assert!(!is_rpm_downgrade("1.2.0-r0", "1.3.0-r0"));
        assert!(!is_rpm_downgrade("1.2.0-r0", "2.0.0-r0"));
        // Release bump only.
        assert!(!is_rpm_downgrade("1.2.0-r0", "1.2.0-r1"));
    }

    #[test]
    fn backward_moves_are_downgrades() {
        assert!(is_rpm_downgrade("1.3.0-r0", "1.2.0-r0"));
        assert!(is_rpm_downgrade("2.0.0-r0", "1.9.9-r0"));
        assert!(is_rpm_downgrade("1.2.0-r1", "1.2.0-r0"));
    }

    #[test]
    fn equal_versions_are_not_downgrades() {
        assert!(!is_rpm_downgrade("1.2.0-r0", "1.2.0-r0"));
    }

    #[test]
    fn numeric_segments_compare_numerically_not_lexically() {
        // The classic rpm/semver trap: "1.10" > "1.9" despite sorting lower
        // as a string. Getting this wrong would flag a routine upgrade as a
        // downgrade and block the build.
        assert!(!is_rpm_downgrade("1.9.0-r0", "1.10.0-r0"));
        assert!(is_rpm_downgrade("1.10.0-r0", "1.9.0-r0"));
    }

    #[test]
    fn leading_zeros_do_not_change_ordering() {
        assert!(!is_rpm_downgrade("1.02.0-r0", "1.2.0-r0"));
        assert!(!is_rpm_downgrade("1.2.0-r0", "1.02.0-r0"));
    }

    #[test]
    fn tilde_sorts_before_its_release_like_a_prerelease() {
        // rpm's `~` is how our caret expansion encodes pre-releases, so
        // 1.2.0~rc.1 must precede 1.2.0 — moving rc -> final is an upgrade,
        // and final -> rc is a downgrade.
        assert!(!is_rpm_downgrade("1.2.0~rc.1-r0", "1.2.0-r0"));
        assert!(is_rpm_downgrade("1.2.0-r0", "1.2.0~rc.1-r0"));
    }

    #[test]
    fn numeric_outranks_alphabetic() {
        // rpmvercmp: a digit segment beats a letter segment.
        assert!(is_rpm_downgrade("1.2.1-r0", "1.2.a-r0"));
        assert!(!is_rpm_downgrade("1.2.a-r0", "1.2.1-r0"));
    }

    #[test]
    fn a_longer_version_with_equal_prefix_is_newer() {
        assert!(!is_rpm_downgrade("1.2-r0", "1.2.1-r0"));
        assert!(is_rpm_downgrade("1.2.1-r0", "1.2-r0"));
    }

    // ---- RPM realization -------------------------------------------------

    fn dep(name: &str, version: Option<&str>) -> ExtensionDependency {
        ExtensionDependency {
            name: name.to_string(),
            version: version.map(str::to_string),
        }
    }

    fn requires(version: Option<&str>) -> Vec<String> {
        dep("weston-base", version).to_rpm_requires().unwrap()
    }

    #[test]
    fn capability_is_a_virtual_provide_not_the_rpm_name() {
        assert_eq!(rpm_capability("weston-base"), "avocado-ext(weston-base)");
    }

    #[test]
    fn unconstrained_dependency_is_a_bare_requires() {
        assert_eq!(requires(None), vec!["Requires: avocado-ext(weston-base)"]);
        // `*` is a constraint that constrains nothing.
        assert_eq!(
            requires(Some("*")),
            vec!["Requires: avocado-ext(weston-base)"]
        );
    }

    #[test]
    fn simple_comparators_map_one_to_one() {
        assert_eq!(
            requires(Some(">=1.2.0")),
            vec!["Requires: avocado-ext(weston-base) >= 1.2.0"]
        );
        assert_eq!(
            requires(Some("=1.2.0")),
            vec!["Requires: avocado-ext(weston-base) = 1.2.0"]
        );
        assert_eq!(
            requires(Some("<2.0.0")),
            vec!["Requires: avocado-ext(weston-base) < 2.0.0"]
        );
    }

    #[test]
    fn caret_expands_to_explicit_bounds() {
        assert_eq!(
            requires(Some("^1.2.3")),
            vec![
                "Requires: avocado-ext(weston-base) >= 1.2.3",
                "Requires: avocado-ext(weston-base) < 2.0.0",
            ]
        );
        // Leading zeros narrow the compatible range.
        assert_eq!(
            requires(Some("^0.2.3")),
            vec![
                "Requires: avocado-ext(weston-base) >= 0.2.3",
                "Requires: avocado-ext(weston-base) < 0.3.0",
            ]
        );
        assert_eq!(
            requires(Some("^0.0.3")),
            vec![
                "Requires: avocado-ext(weston-base) >= 0.0.3",
                "Requires: avocado-ext(weston-base) < 0.0.4",
            ]
        );
    }

    #[test]
    fn tilde_bounds_the_minor() {
        assert_eq!(
            requires(Some("~1.2.3")),
            vec![
                "Requires: avocado-ext(weston-base) >= 1.2.3",
                "Requires: avocado-ext(weston-base) < 1.3.0",
            ]
        );
        assert_eq!(
            requires(Some("~1")),
            vec![
                "Requires: avocado-ext(weston-base) >= 1.0.0",
                "Requires: avocado-ext(weston-base) < 2.0.0",
            ]
        );
    }

    #[test]
    fn multi_comparator_range_emits_both_bounds() {
        assert_eq!(
            requires(Some(">=1.2, <2")),
            vec![
                "Requires: avocado-ext(weston-base) >= 1.2.0",
                "Requires: avocado-ext(weston-base) < 2.0.0",
            ]
        );
    }

    #[test]
    fn bare_version_is_caret_semantics() {
        // Documented gotcha: `1.2.0` means ^1.2.0, not an exact pin.
        assert_eq!(
            requires(Some("1.2.0")),
            vec![
                "Requires: avocado-ext(weston-base) >= 1.2.0",
                "Requires: avocado-ext(weston-base) < 2.0.0",
            ]
        );
    }

    #[test]
    fn prerelease_lower_bound_uses_rpm_tilde_ordering() {
        // RPM sorts `1.2.0~rc.1` before `1.2.0`, matching semver's rule that a
        // pre-release precedes its release.
        assert_eq!(
            requires(Some(">=1.2.0-rc.1")),
            vec!["Requires: avocado-ext(weston-base) >= 1.2.0~rc.1"]
        );
    }

    // ---- runtime list: parent-first, author wins -------------------------

    /// The deptest fixture shape: base <- app-a, base <- mid <- app-b.
    fn fixture_graph() -> DependencyGraph {
        graph(
            "extensions:\n\
             \x20 base: { class: platform }\n\
             \x20 mid:  { class: platform, depends_on: [base] }\n\
             \x20 app-a: { depends_on: [base] }\n\
             \x20 app-b: { depends_on: [mid] }\n\
             \x20 plain: {}\n",
        )
    }

    fn names(entries: &[RuntimeEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn author_places_only_parents_and_deps_fall_under_them() {
        let g = fixture_graph();
        let out = g.resolve_runtime_list(&roots(&["app-a", "app-b"])).unwrap();
        // Each parent immediately followed by what it pulls in.
        assert_eq!(names(&out), vec!["app-a", "base", "app-b", "mid"]);
        assert_eq!(
            out.iter().map(|e| e.authored).collect::<Vec<_>>(),
            vec![true, false, true, false]
        );
    }

    #[test]
    fn parent_outranks_the_dependency_it_pulled_in() {
        // Merge priority is index-based (earlier = higher), so the dependent
        // must precede its dependency or a base could shadow its app.
        let g = fixture_graph();
        let out = g.resolve_runtime_list(&roots(&["app-a"])).unwrap();
        let pos = |n: &str| names(&out).iter().position(|x| *x == n).unwrap();
        assert!(pos("app-a") < pos("base"));
    }

    #[test]
    fn shared_dependency_appears_once_at_its_first_use() {
        let g = fixture_graph();
        let out = g.resolve_runtime_list(&roots(&["app-a", "app-b"])).unwrap();
        assert_eq!(names(&out).iter().filter(|n| **n == "base").count(), 1);
        // Placed under app-a, the first parent to need it — not repeated
        // under mid.
        assert_eq!(names(&out)[1], "base");
    }

    #[test]
    fn explicitly_placed_dependency_keeps_the_authors_position() {
        // The author put `base` last even though app-a implies it. Theirs is
        // truth: one entry, at their position, not hoisted under app-a.
        let g = fixture_graph();
        let out = g
            .resolve_runtime_list(&roots(&["app-a", "app-b", "base"]))
            .unwrap();
        assert_eq!(names(&out), vec!["app-a", "app-b", "mid", "base"]);
        assert_eq!(names(&out).iter().filter(|n| **n == "base").count(), 1);
        // …and it is reported as authored, not implied.
        assert!(out.iter().find(|e| e.name == "base").unwrap().authored);
    }

    #[test]
    fn explicitly_placing_a_transitive_dependency_also_wins() {
        let g = fixture_graph();
        let out = g.resolve_runtime_list(&roots(&["mid", "app-b"])).unwrap();
        // `mid` authored first, pulling base under it; app-b then adds nothing
        // because mid is already placed.
        assert_eq!(names(&out), vec!["mid", "base", "app-b"]);
        assert!(out.iter().find(|e| e.name == "mid").unwrap().authored);
    }

    #[test]
    fn extensions_without_dependencies_are_untouched() {
        let g = fixture_graph();
        let out = g.resolve_runtime_list(&roots(&["plain", "app-a"])).unwrap();
        assert_eq!(names(&out), vec!["plain", "app-a", "base"]);
    }

    #[test]
    fn duplicate_author_entries_collapse_to_the_first() {
        let g = fixture_graph();
        let out = g.resolve_runtime_list(&roots(&["app-a", "app-a"])).unwrap();
        assert_eq!(names(&out), vec!["app-a", "base"]);
    }

    #[test]
    fn runtime_list_reports_the_same_failures_as_the_closure() {
        let g = graph(
            "extensions:\n\
             \x20 app-a: { depends_on: [ghost] }\n",
        );
        let err = g
            .resolve_runtime_list(&roots(&["app-a"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("'ghost' is not defined"), "{err}");
        assert!(err.contains("app-a -> ghost"), "{err}");
    }

    #[test]
    fn runtime_list_rejects_a_cycle() {
        let g = graph(
            "extensions:\n\
             \x20 a: { depends_on: [b] }\n\
             \x20 b: { depends_on: [a] }\n",
        );
        assert!(g.resolve_runtime_list(&roots(&["a"])).is_err());
    }

    // ---- source types: inline / path / git / package ---------------------

    #[test]
    fn merged_remote_extension_resolves_like_any_other() {
        // Once fetched, a git/package extension's own avocado.yaml has been
        // merged, so `depends_on` is present and it is an ordinary node.
        let g = graph(
            "extensions:\n\
             \x20 weston-base: { class: platform }\n\
             \x20 app-a:\n\
             \x20   source: { type: package, version: \"1.0.0\" }\n\
             \x20   depends_on: [weston-base]\n",
        );
        assert_eq!(g.get("app-a").unwrap().origin, ExtensionOrigin::Defined);
        let c = g.resolve(&roots(&["app-a"])).unwrap();
        assert_eq!(c.order, vec!["weston-base", "app-a"]);
    }

    #[test]
    fn unfetched_package_root_errors_instead_of_resolving_empty() {
        // The stub carries no `depends_on`, which looks exactly like "has no
        // dependencies". Resolving it as such would ship an image missing its
        // platform base.
        let g = graph_unfetched(
            "extensions:\n\
             \x20 app-a:\n\
             \x20   source: { type: package, version: \"1.0.0\" }\n",
            &["app-a"],
        );
        let err = g.resolve(&roots(&["app-a"])).unwrap_err().to_string();
        assert!(err.contains("app-a"), "{err}");
        assert!(err.contains("type: package"), "{err}");
        assert!(err.contains("avocado ext fetch"), "{err}");
    }

    #[test]
    fn unfetched_git_dependency_names_the_chain() {
        let g = graph_unfetched(
            "extensions:\n\
             \x20 app-a: { depends_on: [weston-base] }\n\
             \x20 weston-base:\n\
             \x20   source: { type: git, url: \"https://example.invalid/x.git\" }\n",
            &["weston-base"],
        );
        let err = g.resolve(&roots(&["app-a"])).unwrap_err().to_string();
        assert!(err.contains("app-a -> weston-base"), "{err}");
        assert!(err.contains("type: git"), "{err}");
    }

    #[test]
    fn unreadable_path_source_suggests_checking_the_path_not_fetching() {
        // `type: path` is read straight off the host at compose time, so a
        // missing merge means a bad path, not a missing fetch.
        let g = graph_unfetched(
            "extensions:\n\
             \x20 app-a: { depends_on: [local-base] }\n\
             \x20 local-base:\n\
             \x20   source: { type: path, path: \"../local-base\" }\n",
            &["local-base"],
        );
        let err = g.resolve(&roots(&["app-a"])).unwrap_err().to_string();
        assert!(err.contains("no readable avocado.yaml"), "{err}");
        assert!(!err.contains("avocado ext fetch"), "{err}");
    }

    #[test]
    fn unfetched_extension_outside_the_closure_is_harmless() {
        // Only extensions actually reached by the walk are checked — an
        // unrelated unfetched extension must not fail an unrelated build.
        let g = graph_unfetched(
            "extensions:\n\
             \x20 app-a: {}\n\
             \x20 unrelated:\n\
             \x20   source: { type: package, version: \"1.0.0\" }\n",
            &["unrelated"],
        );
        let c = g.resolve(&roots(&["app-a"])).unwrap();
        assert_eq!(c.order, vec!["app-a"]);
    }

    // ---- lints -----------------------------------------------------------

    #[test]
    fn app_depending_on_app_warns() {
        let g = graph(
            "extensions:\n\
             \x20 app-b: {}\n\
             \x20 app-a: { depends_on: [app-b] }\n",
        );
        let c = g.resolve(&roots(&["app-a"])).unwrap();
        let w = g.lint(&c);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("app-a"), "{}", w[0]);
        assert!(w[0].contains("app-b"), "{}", w[0]);
    }

    #[test]
    fn app_depending_on_platform_is_clean() {
        let g = graph(
            "extensions:\n\
             \x20 weston-base: { class: platform }\n\
             \x20 app-a: { depends_on: [weston-base] }\n",
        );
        let c = g.resolve(&roots(&["app-a"])).unwrap();
        assert!(g.lint(&c).is_empty());
    }

    #[test]
    fn platform_on_platform_is_clean() {
        let g = graph(
            "extensions:\n\
             \x20 base: { class: platform }\n\
             \x20 mid: { class: platform, depends_on: [base] }\n",
        );
        let c = g.resolve(&roots(&["mid"])).unwrap();
        assert!(g.lint(&c).is_empty());
    }

    // ---- misc ------------------------------------------------------------

    #[test]
    fn extensions_without_depends_on_resolve_to_themselves() {
        let g = graph("extensions:\n \x20app-a: { version: \"0.4.0\" }\n");
        let c = g.resolve(&roots(&["app-a"])).unwrap();
        assert_eq!(c.order, vec!["app-a"]);
        assert!(c.implied().is_empty());
        assert_eq!(g.get("app-a").unwrap().version.as_deref(), Some("0.4.0"));
    }

    #[test]
    fn missing_extensions_section_yields_an_empty_graph() {
        let g =
            DependencyGraph::from_extensions_section(&Value::Null, "qemux86-64", &HashSet::new())
                .unwrap();
        assert!(g.is_empty());
        assert!(g.resolve(&[]).unwrap().order.is_empty());
    }
}

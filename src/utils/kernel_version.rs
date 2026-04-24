//! Kernel version specification and resolution.
//!
//! Supports splat (`5.15.*`), dnf-style bounded (`>= 6.6`, `< 5.16, >= 5.15`),
//! and exact (`5.15.185-l4t-r36.5-1033.33`) constraints. Resolution picks the
//! highest matching KERNEL_VERSION from a list (typically obtained via
//! `dnf repoquery`), using an `rpmvercmp`-style segment comparator.
//!
//! The resolved version is then used by callers to rewrite kernel-family
//! package names (`kernel*`, `kernel-module-*`, `kernel-devsrc*`,
//! `nv-kernel-module-*`) to include the version suffix so dnf resolves
//! unambiguously even when multiple kernels coexist in a rolling feed.

use anyhow::{anyhow, bail, Result};
use std::cmp::Ordering;

/// A single constraint clause in a bounded spec (e.g. `>= 6.6`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundClause {
    pub op: Operator,
    pub version: String,
}

/// Comparison operators for bounded specs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operator {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// Parsed kernel version constraint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KernelVersionSpec {
    /// Exact match, e.g. `"5.15.185-l4t-r36.5-1033.33"`.
    Exact(String),
    /// Dot-segment prefix glob, e.g. `"5.15.*"` — matches any version starting
    /// with the literal prefix before `*`.
    Glob(String),
    /// One or more `op version` clauses, AND-ed together.
    Bounded(Vec<BoundClause>),
}

impl KernelVersionSpec {
    /// Parse a user-supplied version string. `None` means "latest" — callers
    /// should treat the spec being absent as "accept any version, pick highest".
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        if input.is_empty() {
            bail!("kernel version spec is empty");
        }

        // If any clause operator is present, parse as bounded (even multi-clause).
        if input.contains(">=")
            || input.contains("<=")
            || input.contains("!=")
            || starts_with_op(input)
            || contains_comma_op(input)
        {
            return parse_bounded(input);
        }

        // Glob: contains a `*`. We only accept `*` as a suffix marker (prefix glob).
        if let Some(stripped) = input.strip_suffix('*') {
            // `5.15.*` → prefix `5.15.`
            // `*` alone → prefix `""` (matches anything; equivalent to "latest")
            return Ok(KernelVersionSpec::Glob(stripped.to_string()));
        }
        if input.contains('*') {
            bail!(
                "kernel version glob must end with `*` (got `{input}`); \
                 embedded `*` is not supported"
            );
        }

        Ok(KernelVersionSpec::Exact(input.to_string()))
    }

    /// Returns true if `candidate` satisfies this spec.
    pub fn matches(&self, candidate: &str) -> bool {
        match self {
            KernelVersionSpec::Exact(v) => rpmvercmp(candidate, v) == Ordering::Equal,
            KernelVersionSpec::Glob(prefix) => candidate.starts_with(prefix.as_str()),
            KernelVersionSpec::Bounded(clauses) => clauses.iter().all(|c| {
                let ord = rpmvercmp(candidate, &c.version);
                match c.op {
                    Operator::Eq => ord == Ordering::Equal,
                    Operator::Ne => ord != Ordering::Equal,
                    Operator::Lt => ord == Ordering::Less,
                    Operator::Le => ord != Ordering::Greater,
                    Operator::Gt => ord == Ordering::Greater,
                    Operator::Ge => ord != Ordering::Less,
                }
            }),
        }
    }
}

fn starts_with_op(s: &str) -> bool {
    s.starts_with('<') || s.starts_with('>') || s.starts_with('=')
}

fn contains_comma_op(s: &str) -> bool {
    s.split(',').any(|clause| {
        let t = clause.trim();
        starts_with_op(t)
    })
}

fn parse_bounded(input: &str) -> Result<KernelVersionSpec> {
    let mut clauses = Vec::new();
    for raw in input.split(',') {
        let clause = raw.trim();
        if clause.is_empty() {
            continue;
        }
        let (op, rest) = if let Some(rest) = clause.strip_prefix(">=") {
            (Operator::Ge, rest)
        } else if let Some(rest) = clause.strip_prefix("<=") {
            (Operator::Le, rest)
        } else if let Some(rest) = clause.strip_prefix("!=") {
            (Operator::Ne, rest)
        } else if let Some(rest) = clause.strip_prefix('>') {
            (Operator::Gt, rest)
        } else if let Some(rest) = clause.strip_prefix('<') {
            (Operator::Lt, rest)
        } else if let Some(rest) = clause.strip_prefix('=') {
            (Operator::Eq, rest)
        } else {
            bail!(
                "kernel version clause `{clause}` is missing an operator \
                 (expected one of >=, <=, !=, >, <, =)"
            );
        };
        let version = rest.trim().to_string();
        if version.is_empty() {
            bail!("kernel version clause `{clause}` has no version after operator");
        }
        clauses.push(BoundClause { op, version });
    }
    if clauses.is_empty() {
        bail!("bounded kernel version spec has no clauses");
    }
    Ok(KernelVersionSpec::Bounded(clauses))
}

/// Pick the highest `available` version that satisfies `spec`. When `spec` is
/// `None`, the highest version overall wins.
pub fn resolve_kernel_version(
    spec: Option<&KernelVersionSpec>,
    available: &[String],
) -> Result<String> {
    if available.is_empty() {
        bail!("no kernel versions found in the repository to choose from");
    }

    let mut matches: Vec<&String> = match spec {
        None => available.iter().collect(),
        Some(s) => available.iter().filter(|v| s.matches(v)).collect(),
    };

    if matches.is_empty() {
        return Err(anyhow!(
            "no kernel version in the repository satisfies `{:?}`; available: {}",
            spec,
            available.join(", ")
        ));
    }

    matches.sort_by(|a, b| rpmvercmp(a, b));
    Ok(matches
        .last()
        .expect("non-empty matches after sort")
        .to_string())
}

/// Names whose dnf resolution should be pinned to the selected kernel version.
/// A package name matches the kernel family if it equals `kernel`, starts with
/// `kernel-`, or starts with `nv-kernel-` (meta-tegra OOT shims). This matches
/// `kernel`, `kernel-image`, `kernel-module-*`, `kernel-modules`, `kernel-dev`,
/// `kernel-devsrc*`, `nv-kernel-module-*`.
pub fn is_kernel_family(name: &str) -> bool {
    name == "kernel" || name.starts_with("kernel-") || name.starts_with("nv-kernel-")
}

/// Rewrite any kernel-family keys in `packages` to include the resolved kernel
/// version suffix so dnf resolution is unambiguous when multiple kernels
/// coexist in a rolling feed. Non-kernel-family keys pass through unchanged.
///
/// Names that already end with the exact `-<kernel_version>` suffix are left
/// alone (prevents double-suffixing if a user explicitly writes the versioned
/// name into their avocado.yaml).
///
/// Generic over the value type so this can be applied to any `HashMap<String, _>`
/// representation of a packages section.
pub fn rewrite_kernel_family_packages<V: Clone>(
    packages: &std::collections::HashMap<String, V>,
    kernel_version: &str,
) -> std::collections::HashMap<String, V> {
    let suffix = format!("-{kernel_version}");
    let mut out = std::collections::HashMap::with_capacity(packages.len());
    for (name, value) in packages {
        let new_name = if is_kernel_family(name) && !name.ends_with(&suffix) {
            format!("{name}{suffix}")
        } else {
            name.clone()
        };
        out.insert(new_name, value.clone());
    }
    out
}

/// Compare two versions using a close approximation of RPM's `rpmvercmp`
/// algorithm: separate into runs of digits / runs of alphas split by
/// non-alphanumeric characters, compare segment-by-segment with numeric runs
/// winning over alpha, `~` (tilde) sorting before absence, `^` (caret) after
/// absence. Good enough for KERNEL_VERSION ordering; not a full libsolv /
/// `rpmlib` reimplementation.
pub fn rpmvercmp(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }

    let mut av = a.as_bytes();
    let mut bv = b.as_bytes();

    loop {
        // Strip leading non-alnum, except honor `~` and `^` before stripping.
        if let Some(ord) = cmp_special_prefix(&mut av, &mut bv) {
            return ord;
        }
        strip_nonalnum_nonspecial(&mut av);
        strip_nonalnum_nonspecial(&mut bv);

        if av.is_empty() || bv.is_empty() {
            return match (av.is_empty(), bv.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                _ => unreachable!(),
            };
        }

        let a_numeric = av[0].is_ascii_digit();
        let b_numeric = bv[0].is_ascii_digit();

        if a_numeric != b_numeric {
            // Numeric segments outrank alpha.
            return if a_numeric {
                Ordering::Greater
            } else {
                Ordering::Less
            };
        }

        let a_seg = take_segment(&mut av, a_numeric);
        let b_seg = take_segment(&mut bv, b_numeric);

        let ord = if a_numeric {
            // Strip leading zeros.
            let a_trim = trim_leading_zeros(a_seg);
            let b_trim = trim_leading_zeros(b_seg);
            a_trim
                .len()
                .cmp(&b_trim.len())
                .then_with(|| a_trim.cmp(b_trim))
        } else {
            a_seg.cmp(b_seg)
        };

        if ord != Ordering::Equal {
            return ord;
        }
        // Segments equal — continue.
    }
}

fn cmp_special_prefix(av: &mut &[u8], bv: &mut &[u8]) -> Option<Ordering> {
    let a_tilde = av.first() == Some(&b'~');
    let b_tilde = bv.first() == Some(&b'~');
    if a_tilde || b_tilde {
        if a_tilde && !b_tilde {
            return Some(Ordering::Less);
        }
        if !a_tilde && b_tilde {
            return Some(Ordering::Greater);
        }
        *av = &av[1..];
        *bv = &bv[1..];
        return None;
    }
    let a_caret = av.first() == Some(&b'^');
    let b_caret = bv.first() == Some(&b'^');
    if a_caret || b_caret {
        // `^` sorts after absence but before nothing else — treat symmetric carets
        // as equal-and-advance; mismatched carets: the one without caret is "less"
        // (pre-caret), the one with caret is "greater".
        if a_caret && bv.is_empty() {
            return Some(Ordering::Greater);
        }
        if b_caret && av.is_empty() {
            return Some(Ordering::Less);
        }
        if a_caret && !b_caret {
            return Some(Ordering::Greater);
        }
        if !a_caret && b_caret {
            return Some(Ordering::Less);
        }
        *av = &av[1..];
        *bv = &bv[1..];
        return None;
    }
    None
}

fn strip_nonalnum_nonspecial(s: &mut &[u8]) {
    while let Some(&c) = s.first() {
        if c.is_ascii_alphanumeric() || c == b'~' || c == b'^' {
            break;
        }
        *s = &s[1..];
    }
}

fn take_segment<'a>(s: &mut &'a [u8], numeric: bool) -> &'a [u8] {
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        let matches = if numeric {
            c.is_ascii_digit()
        } else {
            c.is_ascii_alphabetic()
        };
        if !matches {
            break;
        }
        i += 1;
    }
    let (seg, rest) = s.split_at(i);
    *s = rest;
    seg
}

fn trim_leading_zeros(s: &[u8]) -> &[u8] {
    let mut i = 0;
    while i < s.len() - 1 && s[i] == b'0' {
        i += 1;
    }
    &s[i..]
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- rpmvercmp ---------------------------------------------------------

    #[test]
    fn rpmvercmp_equal() {
        assert_eq!(rpmvercmp("5.15.185", "5.15.185"), Ordering::Equal);
    }

    #[test]
    fn rpmvercmp_numeric_greater() {
        assert_eq!(rpmvercmp("5.15.185", "5.15.148"), Ordering::Greater);
        assert_eq!(rpmvercmp("5.15.148", "5.15.185"), Ordering::Less);
        assert_eq!(rpmvercmp("6.6.123", "5.15.999"), Ordering::Greater);
    }

    #[test]
    fn rpmvercmp_tegra_style() {
        assert_eq!(
            rpmvercmp("5.15.185-l4t-r36.5-1033.33", "5.15.148-l4t-r36.4.4-1012.12"),
            Ordering::Greater
        );
    }

    #[test]
    fn rpmvercmp_srcpv_suffix() {
        assert_eq!(
            rpmvercmp("5.15.185+git0+9c6d5c8154", "5.15.148+git0+c8a827653"),
            Ordering::Greater
        );
    }

    #[test]
    fn rpmvercmp_leading_zeros() {
        // 10 and 010 should compare equal when numeric segments strip leading zeros
        assert_eq!(rpmvercmp("1.010", "1.10"), Ordering::Equal);
    }

    // --- spec parse --------------------------------------------------------

    #[test]
    fn parse_exact() {
        assert_eq!(
            KernelVersionSpec::parse("5.15.185").unwrap(),
            KernelVersionSpec::Exact("5.15.185".to_string())
        );
    }

    #[test]
    fn parse_glob() {
        assert_eq!(
            KernelVersionSpec::parse("5.15.*").unwrap(),
            KernelVersionSpec::Glob("5.15.".to_string())
        );
    }

    #[test]
    fn parse_embedded_glob_rejected() {
        assert!(KernelVersionSpec::parse("5.*.185").is_err());
    }

    #[test]
    fn parse_bounded_single() {
        let spec = KernelVersionSpec::parse(">= 6.6").unwrap();
        match spec {
            KernelVersionSpec::Bounded(clauses) => {
                assert_eq!(clauses.len(), 1);
                assert_eq!(clauses[0].op, Operator::Ge);
                assert_eq!(clauses[0].version, "6.6");
            }
            _ => panic!("expected Bounded"),
        }
    }

    #[test]
    fn parse_bounded_multi() {
        let spec = KernelVersionSpec::parse(">= 5.15, < 6").unwrap();
        match spec {
            KernelVersionSpec::Bounded(clauses) => {
                assert_eq!(clauses.len(), 2);
                assert_eq!(clauses[0].op, Operator::Ge);
                assert_eq!(clauses[1].op, Operator::Lt);
            }
            _ => panic!("expected Bounded"),
        }
    }

    #[test]
    fn parse_missing_version() {
        assert!(KernelVersionSpec::parse(">=").is_err());
    }

    #[test]
    fn parse_empty() {
        assert!(KernelVersionSpec::parse("").is_err());
        assert!(KernelVersionSpec::parse("   ").is_err());
    }

    // --- spec matches ------------------------------------------------------

    #[test]
    fn glob_matches_tegra() {
        let spec = KernelVersionSpec::parse("5.15.*").unwrap();
        assert!(spec.matches("5.15.185-l4t-r36.5-1033.33"));
        assert!(spec.matches("5.15.148-l4t-r36.4.4-1012.12"));
        assert!(!spec.matches("6.6.123"));
    }

    #[test]
    fn glob_star_alone_matches_all() {
        let spec = KernelVersionSpec::parse("*").unwrap();
        assert!(spec.matches("anything-goes"));
    }

    #[test]
    fn bounded_ge_matches() {
        let spec = KernelVersionSpec::parse(">= 6.6").unwrap();
        assert!(spec.matches("6.6.123"));
        assert!(spec.matches("6.7.0"));
        assert!(!spec.matches("5.15.185"));
    }

    #[test]
    fn bounded_range_matches() {
        let spec = KernelVersionSpec::parse(">= 5.15, < 6").unwrap();
        assert!(spec.matches("5.15.185"));
        assert!(spec.matches("5.15.148"));
        assert!(!spec.matches("6.6.123"));
        assert!(!spec.matches("5.14.0"));
    }

    // --- resolve -----------------------------------------------------------

    #[test]
    fn resolve_picks_highest() {
        let versions = vec![
            "5.15.148-l4t-r36.4.4-1012.12".to_string(),
            "5.15.185-l4t-r36.5-1033.33".to_string(),
            "6.6.123".to_string(),
        ];
        let spec = KernelVersionSpec::parse("5.15.*").unwrap();
        assert_eq!(
            resolve_kernel_version(Some(&spec), &versions).unwrap(),
            "5.15.185-l4t-r36.5-1033.33"
        );
    }

    #[test]
    fn resolve_latest_when_none() {
        let versions = vec!["5.15.185".to_string(), "6.6.123".to_string()];
        assert_eq!(resolve_kernel_version(None, &versions).unwrap(), "6.6.123");
    }

    #[test]
    fn resolve_no_match() {
        let versions = vec!["5.15.185".to_string()];
        let spec = KernelVersionSpec::parse(">= 6.6").unwrap();
        assert!(resolve_kernel_version(Some(&spec), &versions).is_err());
    }

    #[test]
    fn resolve_empty_available() {
        let spec = KernelVersionSpec::parse("*").unwrap();
        assert!(resolve_kernel_version(Some(&spec), &[]).is_err());
    }

    // --- is_kernel_family --------------------------------------------------

    #[test]
    fn kernel_family_names() {
        assert!(is_kernel_family("kernel"));
        assert!(is_kernel_family("kernel-image"));
        assert!(is_kernel_family("kernel-module-host1x"));
        assert!(is_kernel_family("kernel-modules"));
        assert!(is_kernel_family("kernel-devsrc"));
        assert!(is_kernel_family("nv-kernel-module-host1x"));
        assert!(!is_kernel_family("busybox"));
        assert!(!is_kernel_family("kernellabs"));
    }

    // --- rewrite -----------------------------------------------------------

    #[test]
    fn rewrite_adds_suffix_to_kernel_family() {
        let mut pkgs = std::collections::HashMap::new();
        pkgs.insert("kernel-module-host1x".to_string(), "*".to_string());
        pkgs.insert("kernel-devsrc".to_string(), "*".to_string());
        pkgs.insert("busybox".to_string(), "*".to_string());
        pkgs.insert(
            "nv-kernel-module-watchdog-tegra-t18x".to_string(),
            "*".to_string(),
        );

        let kver = "5.15.185-l4t-r36.5-1033.33";
        let out = rewrite_kernel_family_packages(&pkgs, kver);

        assert!(out.contains_key("kernel-module-host1x-5.15.185-l4t-r36.5-1033.33"));
        assert!(out.contains_key("kernel-devsrc-5.15.185-l4t-r36.5-1033.33"));
        assert!(out.contains_key("nv-kernel-module-watchdog-tegra-t18x-5.15.185-l4t-r36.5-1033.33"));
        assert!(out.contains_key("busybox"));
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn rewrite_does_not_double_suffix() {
        let mut pkgs = std::collections::HashMap::new();
        pkgs.insert(
            "kernel-module-host1x-5.15.185-l4t-r36.5-1033.33".to_string(),
            "*".to_string(),
        );

        let kver = "5.15.185-l4t-r36.5-1033.33";
        let out = rewrite_kernel_family_packages(&pkgs, kver);

        assert!(out.contains_key("kernel-module-host1x-5.15.185-l4t-r36.5-1033.33"));
        assert!(!out.contains_key(&format!(
            "kernel-module-host1x-5.15.185-l4t-r36.5-1033.33-{kver}"
        )));
    }
}

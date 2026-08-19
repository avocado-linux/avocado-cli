//! `avocado sbom` — emit an SPDX 3.0.1 SBOM of what this project installed.
//!
//! Asks RPM what is in every sysroot (the walk lives in `utils::sysroot_scan`)
//! and writes one `software_Sbom` of type `deployed`.
//!
//! One document with one root per scope, rather than one document per scope,
//! for a reason that is about correctness rather than tidiness: a package
//! present in both `rootfs` and `initramfs` is one package in two places. Split
//! across files it becomes two SPDX elements, and a scanner counting packages
//! or matching CVEs counts the same exposure twice. Here it is one element with
//! two `contains` relationships.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::utils::config::{ComposedConfig, Config};
use crate::utils::lockfile::{LockFile, RepoSnapshot, RPM_SBOM_FIELDS, RPM_SBOM_FORMAT};
use crate::utils::output::{print_info, print_success, OutputLevel};
use crate::utils::output_format::{emit_json_object, JsonOutputGuard, OutputFormat};
use crate::utils::sysroot_scan::{self, ScanRequest};
use crate::utils::target::resolve_target_required;

/// Version of the SPDX specification the emitted document declares.
const SPEC_VERSION: &str = "3.0.1";

/// JSON-LD context for that version.
const CONTEXT: &str = "https://spdx.org/rdf/3.0.1/spdx-context.jsonld";

/// Scopes that exist on the build host and ship nothing to a device. Auditing
/// them is a legitimate thing to want — hence `--include-sdk` — but a document
/// answering "what is on this device" must not list the cross toolchain by
/// default: it is 290 `nativesdk-*` packages against the ~400 that ship.
const BUILD_HOST_SCOPES: &[&str] = &["sdk", "target-sysroot"];

/// One installed package, with the provenance SPDX asks for.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Package {
    name: String,
    version: String,
    release: String,
    arch: String,
    epoch: String,
    license: String,
    url: String,
    packager: String,
    sourcerpm: String,
    sha256: String,
    buildtime: String,
    installtid: String,
    summary: String,
}

impl Package {
    /// Identity for de-duplication across scopes. Deliberately excludes the
    /// scope: the same package in two sysroots is one package. Includes the
    /// epoch, without which `1:2.39-r0.2` and `2.39-r0.2` — different packages
    /// by rpm's own ordering — would collapse into one element.
    fn key(&self) -> (&str, &str, &str, &str, &str) {
        (
            &self.name,
            self.epoch(),
            &self.version,
            &self.release,
            &self.arch,
        )
    }

    /// The epoch as a comparable string, `""` when rpm reports none. rpm writes
    /// `(none)` for an unset epoch, which is not the same as epoch 0 in the
    /// tag but compares equal for every purpose here.
    fn epoch(&self) -> &str {
        present(&self.epoch).unwrap_or("")
    }

    /// `2.39-r0.2`, or `1:2.39-r0.2` when the package carries an epoch. This is
    /// the version a human reads and the one rpm compares on.
    fn evr(&self) -> String {
        match self.epoch() {
            "" => format!("{}-{}", self.version, self.release),
            e => format!("{}:{}-{}", e, self.version, self.release),
        }
    }

    /// The package URL a consumer matches on.
    ///
    /// Nothing in the SPDX shapes validates this field — a malformed one passes
    /// validation and fails silently downstream — so the components are encoded
    /// here rather than interpolated raw.
    ///
    /// The epoch travels as the `epoch` qualifier rather than inside the
    /// version, which is what the purl spec reserves for it; qualifiers are
    /// emitted in the sorted order the spec requires, so `arch` precedes it.
    /// The `arch` qualifier is dropped when rpm has none to report rather than
    /// carrying `(none)`. A qualifier is a claim a scanner matches on, and
    /// `?arch=(none)` matches no purl anything else derives from the same rpm —
    /// it is worse than the absent qualifier, which at least degrades to a
    /// name/version match. `gpg-pubkey`, which dnf installs into an installroot
    /// whenever it imports a repo signing key, is the row that has neither an
    /// arch nor a source rpm.
    fn purl(&self) -> String {
        let mut purl = format!(
            "pkg:rpm/avocado/{}@{}-{}",
            purl_encode(&self.name),
            purl_encode(&self.version),
            purl_encode(&self.release),
        );
        let mut sep = '?';
        if let Some(arch) = present(&self.arch) {
            purl.push_str(&format!("{sep}arch={}", purl_encode(arch)));
            sep = '&';
        }
        if !self.epoch().is_empty() {
            purl.push_str(&format!("{sep}epoch={}", purl_encode(self.epoch())));
        }
        purl
    }

    /// `glibc-2.39+git0+ce65d944e3-r0.2.src.rpm` -> `glibc`.
    ///
    /// The last two dash-separated fields are VERSION and RELEASE, so the
    /// recipe is everything before them. Split from the right rather than
    /// matched, because the version carries dashes of its own in the
    /// `2.39+git0+...` form Yocto writes for a git recipe.
    ///
    /// Worth having: it means a device SBOM can name the recipe behind a
    /// package without being joined against the build's pkgdata.
    /// `None` when rpm reports no source rpm: the fallback is the stem, and for
    /// an unset tag the stem is the literal `(none)`.
    fn recipe(&self) -> Option<&str> {
        let stem = present(&self.sourcerpm)?
            .strip_suffix(".src.rpm")
            .unwrap_or_else(|| self.sourcerpm.trim());
        match stem.rsplitn(3, '-').nth(2) {
            Some(recipe) if !recipe.is_empty() => Some(recipe),
            _ => present(stem),
        }
    }
}

/// A sysroot and the packages it holds that are its own.
#[derive(Debug)]
struct Scope {
    name: String,
    root: String,
    packages: Vec<Package>,
    failed: bool,
    /// Rows rpm printed that could not be mapped onto fields. Carried so the
    /// command can say a package is missing: this command refuses a scope it
    /// could not read at all, and a row it could not read is the same claim in
    /// miniature.
    unreadable: usize,
}

/// Whether a scope's installroot was seeded with a copy of the rootfs RPM
/// database, and so needs the seed subtracted before its packages can be read
/// as its own content. See `parse_scopes` for what the seed is and why.
fn is_seeded_scope(name: &str) -> bool {
    name.starts_with("ext:") || name.starts_with("runtime:")
}

/// Tripwires on the seeded-scope subtraction, which is a heuristic and has been
/// wrong before. An installroot seeded from the rootfs holds the whole base, so
/// the last time the subtraction failed it did so silently, on every seeded
/// scope at once, and still produced a plausible-looking SBOM.
///
/// Returns the lines to print rather than printing them, so the conditions can
/// be asserted against a scope list — the caller sits past a container round
/// trip, and a guard that cannot be reached by a test is a guard that can be
/// inverted without anything noticing.
fn seeding_warnings(scopes: &[Scope]) -> Vec<String> {
    let base_count = scopes
        .iter()
        .find(|s| s.name == "rootfs")
        .map_or(0, |s| s.packages.len());
    let seeded: Vec<&str> = scopes
        .iter()
        .filter(|s| is_seeded_scope(&s.name) && !s.packages.is_empty())
        .map(|s| s.name.as_str())
        .collect();

    let mut warnings = Vec::new();

    // The subtraction runs only against rows the rootfs contributed, so a
    // rootfs that contributed none — absent from the dump, or every row of it
    // unreadable — means nothing was subtracted and every seeded scope still
    // carries its whole copy of the base. That is the harmful direction, and
    // the count tripwire below cannot catch it: it needs the rootfs count it no
    // longer has. Said separately for that reason.
    if base_count == 0 && !seeded.is_empty() {
        warnings.push(format!(
            "[WARN] The rootfs contributed no packages, so nothing was subtracted from the {} \
             seeded scope(s): {}. Their installroots are seeded with a copy of the rootfs RPM \
             database, so what they report is the base system plus their own content, not what \
             the extension ships.",
            seeded.len(),
            seeded.join(", ")
        ));
    }

    // A seeded scope reporting as many packages as the rootfs itself is the
    // shape the subtraction failing takes.
    if base_count > 1 {
        let suspect: Vec<&str> = scopes
            .iter()
            .filter(|s| is_seeded_scope(&s.name) && s.packages.len() >= base_count)
            .map(|s| s.name.as_str())
            .collect();
        if !suspect.is_empty() {
            warnings.push(format!(
                "[WARN] {} scope(s) report at least as many packages as the rootfs ({}): {}. \
                 Either they really do install that much, or the rootfs database they were seeded \
                 from could not be told apart from what they installed themselves — check before \
                 treating their contents as shipped by the extension.",
                suspect.len(),
                base_count,
                suspect.join(", ")
            ));
        }
    }

    warnings
}

/// Rewrite a Yocto license string as an SPDX license expression.
///
/// Yocto writes `&` and `|` where SPDX wants `AND` and `OR`, and uses a handful
/// of identifiers the SPDX list either never carried or has since deprecated.
/// Those become `LicenseRef-`, which is the escape hatch the spec provides for
/// exactly this — emitting them verbatim would produce an expression no
/// consumer can resolve.
///
/// An unset tag is `NOASSERTION`. rpm prints `(none)` for one, and without the
/// [`present`] guard the parentheses below are read as structure: `(none)`
/// tokenises to the expression `( none )`, which looks resolvable and is not.
fn spdx_license(raw: &str) -> String {
    let raw = match present(raw) {
        Some(raw) => raw,
        None => return "NOASSERTION".to_string(),
    };

    let mut out: Vec<String> = Vec::new();
    let mut token = String::new();

    let flush = |token: &mut String, out: &mut Vec<String>| {
        let t = token.trim();
        if !t.is_empty() {
            out.push(non_spdx_id(t).to_string());
        }
        token.clear();
    };

    for c in raw.chars() {
        match c {
            '&' => {
                flush(&mut token, &mut out);
                out.push("AND".to_string());
            }
            '|' => {
                flush(&mut token, &mut out);
                out.push("OR".to_string());
            }
            '(' | ')' => {
                flush(&mut token, &mut out);
                out.push(c.to_string());
            }
            _ => token.push(c),
        }
    }
    flush(&mut token, &mut out);

    if out.is_empty() {
        return "NOASSERTION".to_string();
    }
    out.join(" ")
}

/// Identifiers Yocto emits that the SPDX license list does not carry.
///
/// Kept as a table rather than a heuristic: guessing at a license identifier is
/// worse than declaring it unresolvable, since a wrong one is indistinguishable
/// from a right one downstream. Anything not listed is passed through, because
/// deciding "not a real SPDX identifier" needs the license list itself and this
/// command does not carry one — an unrecognised identifier is far more often a
/// listed one this table has no opinion about than a Yocto-ism.
///
/// The entries are the Yocto-isms that reach a device image. `Proprietary` and
/// the `Firmware-*` family matter most: they are what vendor blobs and BSP
/// firmware declare, so they are exactly the packages whose licensing a reader
/// came to the document to check, and emitting them bare produces an expression
/// no consumer can resolve.
fn non_spdx_id(id: &str) -> Cow<'_, str> {
    // Yocto writes one of these per firmware blob — `Firmware-qcom`,
    // `Firmware-amd-ucode`, and so on down the BSP. Listing them individually
    // would be a table that goes stale with every new SoC, and the prefix is
    // unambiguous: no SPDX-listed identifier begins with it.
    if let Some(rest) = id.strip_prefix("Firmware-") {
        return Cow::Owned(format!("LicenseRef-Firmware-{rest}"));
    }

    let mapped = match id {
        "bzip2-1.0.4" => "LicenseRef-bzip2-1.0.4",
        "PD" => "LicenseRef-PD",
        // Yocto's marker for a recipe with no redistributable license. Common
        // enough on a device build to matter, and emitting it bare would read
        // as a listed SPDX identifier that no consumer can resolve.
        "CLOSED" => "LicenseRef-CLOSED",
        // What a vendor blob declares. Not on the SPDX list in any casing.
        "Proprietary" => "LicenseRef-Proprietary",
        "GPL-2.0-with-OpenSSL-exception" => "LicenseRef-GPL-2.0-with-OpenSSL-exception",
        "GPL-3.0-with-GCC-exception" => "LicenseRef-GPL-3.0-with-GCC-exception",
        other => other,
    };
    Cow::Borrowed(mapped)
}

/// Percent-encode the characters that carry structural meaning in a purl.
///
/// Only those: `:` `/` `@` `?` `#` and `%` itself separate a purl's type,
/// namespace, name, version and qualifiers, so leaving one literal moves a
/// component boundary and a parser reads a different package. Everything else
/// stays as written — in particular `+`, which appears in 29 of the 273
/// packages on a real qemuarm64 device (`libstdc++6`, and the `2.39+git0+...`
/// form Yocto gives a git recipe). It is not a purl separator, and it sits
/// ahead of the `?` where a query-string parser could not read it as a space.
///
/// Anything outside printable ASCII is encoded too, since a purl is a URI.
fn purl_encode(component: &str) -> String {
    let mut out = String::with_capacity(component.len());
    for byte in component.bytes() {
        match byte {
            b':' | b'/' | b'@' | b'?' | b'#' | b'%' | b'&' | b'=' => {
                out.push_str(&format!("%{byte:02X}"))
            }
            0x21..=0x7E => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// An id component that is safe to embed in an IRI *and* distinct for distinct
/// inputs.
///
/// [`slug`] alone is not injective: it collapses every run of non-alphanumerics
/// to one `-` and trims the ends, so `GPL-2.0` and `GPL-2.0+` both come out as
/// `GPL-2.0`. Two graph nodes would then share an `spdxId` while carrying
/// different content, and a consumer resolving that id gets whichever it
/// happened to read last — a package reported under the wrong license.
///
/// A string the slug preserves exactly is used as-is, which keeps the common
/// case readable (`busybox-1.36.1-r0.2.cortexa57`). Anything the slug altered
/// gets a digest of the *original* appended, so the mapping is injective again
/// without making every id unreadable.
fn slug_id(s: &str) -> String {
    let base = slug(s);
    if base == s && !base.is_empty() {
        return base;
    }
    let digest = Sha256::digest(s.as_bytes());
    let suffix: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
    if base.is_empty() {
        suffix
    } else {
        format!("{base}-{suffix}")
    }
}

/// The document's `created` timestamp.
///
/// `SOURCE_DATE_EPOCH` wins when it is set and parses, so the same installed
/// set produces the same bytes twice running. `namespace_digest` already holds
/// every element id still for that case; a wall-clock `created` would undo it
/// one field later, and a consumer diffing two SBOMs would see churn that is
/// not there. It also lets anything that pins or signs the document reference
/// it by content hash.
///
/// Unset or unparseable falls back to now, which is the honest answer when the
/// caller has not named a reproducible one. Seconds since the epoch, UTC, as
/// the reproducible-builds convention defines it.
fn created_timestamp(source_date_epoch: Option<&str>) -> String {
    const FORMAT: &str = "%Y-%m-%dT%H:%M:%SZ";
    source_date_epoch
        .and_then(|s| s.trim().parse::<i64>().ok())
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .map(|t| t.format(FORMAT).to_string())
        .unwrap_or_else(|| chrono::Utc::now().format(FORMAT).to_string())
}

/// Make a string safe to embed in an SPDX id, which must be an IRI.
///
/// Not injective — see [`slug_id`], which is what id construction must use.
fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = true;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '.' {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// rpm writes `(none)` rather than an empty string for an unset tag, and an
/// SPDX property carrying the literal `(none)` is worse than an absent one.
fn present(value: &str) -> Option<&str> {
    match value.trim() {
        "" | "(none)" => None,
        v => Some(v),
    }
}

/// Epoch seconds -> the timestamp format SPDX pins: no fractional part, no
/// offset, `Z` only. Anything else is rejected by the spec's own pattern.
fn spdx_time(epoch: &str) -> Option<String> {
    let secs: i64 = epoch.trim().parse().ok()?;
    let dt = chrono::DateTime::from_timestamp(secs, 0)?;
    Some(dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

pub struct SbomCommand {
    config_path: String,
    /// The global `--runs-on`, carried only so the command can refuse it.
    /// `run_in_container_capture` has no remote branch — unlike
    /// `run_in_container`, which routes on `config.runs_on` — so honouring it
    /// would take work in `utils::container`, not a field here.
    runs_on: Option<String>,
    target: Option<String>,
    output_path: Option<String>,
    include_sdk: bool,
    verbose: bool,
    container_args: Option<Vec<String>>,
    output: OutputFormat,
    sdk_arch: Option<String>,
    composed_config: Option<Arc<ComposedConfig>>,
}

impl SbomCommand {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config_path: String,
        target: Option<String>,
        output_path: Option<String>,
        include_sdk: bool,
        verbose: bool,
        container_args: Option<Vec<String>>,
        output: OutputFormat,
    ) -> Self {
        Self {
            config_path,
            target,
            output_path,
            include_sdk,
            verbose,
            container_args,
            output,
            runs_on: None,
            sdk_arch: None,
            composed_config: None,
        }
    }

    /// Record the global `--runs-on` so `execute` can refuse it rather than
    /// silently describing the local machine.
    pub fn with_runs_on(mut self, runs_on: Option<String>) -> Self {
        self.runs_on = runs_on;
        self
    }

    pub fn with_sdk_arch(mut self, sdk_arch: Option<String>) -> Self {
        self.sdk_arch = sdk_arch;
        self
    }

    #[allow(dead_code)]
    pub fn with_composed_config(mut self, config: Arc<ComposedConfig>) -> Self {
        self.composed_config = Some(config);
        self
    }

    pub async fn execute(&self) -> Result<()> {
        // Refused rather than ignored. The container helper this command uses
        // reads the local volume with no remote branch, so
        // accepting it would describe this machine's sysroots in a document
        // named for another host — and an SBOM is a claim about what is on a
        // particular device.
        if let Some(host) = &self.runs_on {
            anyhow::bail!(
                "--runs-on {host} is not supported by `sbom`: it would describe this machine's \
                 sysroots as {host}'s. Run the command on that host."
            );
        }

        let _json_guard = self.output.is_json().then(JsonOutputGuard::enable);

        let composed = match &self.composed_config {
            Some(cc) => Arc::clone(cc),
            None => Arc::new(
                Config::load_composed(&self.config_path, self.target.as_deref())
                    .context("Failed to load composed config")?,
            ),
        };
        let config = &composed.config;
        let target = resolve_target_required(self.target.as_deref(), config)?;

        // Progress goes to stdout (`utils::output::print_info` is a `println!`),
        // and so does the document when no `-o` was given. Left on, `avocado
        // sbom --verbose > sbom.json` writes `[INFO] Querying installed
        // packages...` ahead of the JSON and the file will not parse. The
        // document is the output that matters, so the commentary yields —
        // announced on stderr, which is where a note about stdout belongs.
        let scan_verbose = self.verbose && self.output_path.is_some();
        if self.verbose && !scan_verbose {
            eprintln!(
                "[INFO] --verbose is suppressed while the document goes to stdout; it would be \
                 written into the JSON. Pass -o <path> to see it."
            );
        }

        let output = sysroot_scan::run_discovery(
            config,
            ScanRequest {
                config_path: &self.config_path,
                target: &target,
                verbose: scan_verbose,
                container_args: self.container_args.as_ref(),
                sdk_arch: self.sdk_arch.clone(),
                query_format: RPM_SBOM_FORMAT,
            },
        )
        .await?;

        let scopes = self.parse_scopes(&output.stdout);

        // A scope whose database could not be read contributes no packages. An
        // SBOM silently missing a sysroot is worse than no SBOM: it is a
        // complete-looking inventory of an incomplete scan.
        let failed: Vec<&str> = scopes
            .iter()
            .filter(|s| s.failed)
            .map(|s| s.name.as_str())
            .collect();
        if !failed.is_empty() {
            anyhow::bail!(
                "Could not read the RPM database of {} scope(s): {}. Their packages would be \
                 missing from the SBOM.{}",
                failed.len(),
                failed.join(", "),
                sysroot_scan::stderr_tail(&output.stderr)
            );
        }

        // Not fatal — one unreadable row out of four hundred is a worse reason
        // to produce no SBOM than to produce one short a package. But it is
        // said out loud, on stderr so it survives the document owning stdout,
        // because the alternative is a document that looks complete and is
        // not.
        let unreadable: usize = scopes.iter().map(|s| s.unreadable).sum();
        if unreadable > 0 {
            let where_ = scopes
                .iter()
                .filter(|s| s.unreadable > 0)
                .map(|s| format!("{} ({})", s.name, s.unreadable))
                .collect::<Vec<_>>()
                .join(", ");
            eprintln!(
                "[WARN] {unreadable} package row(s) could not be read and are missing from the \
                 SBOM: {where_}. A tab inside an rpm tag shifts every field after it, so the row \
                 is dropped rather than mapped onto the wrong columns."
            );
        }

        for warning in seeding_warnings(&scopes) {
            eprintln!("{warning}");
        }

        if scopes.iter().all(|s| s.packages.is_empty()) {
            anyhow::bail!(
                "No installed package was found in any sysroot for target '{target}'. Run \
                 `avocado install` first, and note that this command reads the state volume of \
                 the current directory, not of --config's directory."
            );
        }

        // Provenance, and never a reason to fail: the document is built from
        // the RPM databases, and the lockfile only says which feed filled them.
        // A missing or unreadable lockfile costs the reader that pointer, which
        // is worth strictly less than the inventory itself.
        let src_dir = config
            .get_resolved_src_dir(&self.config_path)
            .unwrap_or_else(|| {
                std::path::Path::new(&self.config_path)
                    .parent()
                    .unwrap_or(std::path::Path::new("."))
                    .to_path_buf()
            });
        let snapshot = LockFile::load(&src_dir)
            .ok()
            .and_then(|lock| lock.get_repo_snapshot(&target).cloned());

        let doc = self.build_document(&scopes, &target, snapshot.as_ref());

        match &self.output_path {
            Some(path) => {
                std::fs::write(path, serde_json::to_string_pretty(&doc)?)
                    .with_context(|| format!("Failed to write SBOM to '{path}'"))?;
                self.print_summary(&scopes, Some(path));
            }
            None if self.output.is_json() => emit_json_object(&doc),
            None => {
                println!("{}", serde_json::to_string_pretty(&doc)?);
            }
        }

        Ok(())
    }

    /// Map the raw dump onto packages, dropping what a scope only sees because
    /// its installroot was seeded from the rootfs.
    fn parse_scopes(&self, output: &str) -> Vec<Scope> {
        let dumps = sysroot_scan::parse_scopes(output);

        let to_packages = |rows: &[Vec<String>]| -> Vec<Package> {
            rows.iter()
                // A row with any other field count holds a tab inside one of
                // the fields, or is a truncated read. Either way the offsets
                // below have shifted, and mapping it anyway would put a
                // description where a checksum belongs — so it is dropped and
                // counted. Rejoining the tail into SUMMARY would be right only
                // if SUMMARY were the field that held the tab; when it was
                // LICENSE or PACKAGER instead, that recovers a package with a
                // plausible and wrong checksum, which is worse than losing it.
                .filter(|r| r.len() == RPM_SBOM_FIELDS)
                .map(|r| Package {
                    name: r[0].clone(),
                    version: r[1].clone(),
                    release: r[2].clone(),
                    arch: r[3].clone(),
                    epoch: r[4].clone(),
                    license: r[5].clone(),
                    url: r[6].clone(),
                    packager: r[7].clone(),
                    sourcerpm: r[8].clone(),
                    sha256: r[9].clone(),
                    buildtime: r[10].clone(),
                    installtid: r[11].clone(),
                    summary: r[12].clone(),
                })
                .collect()
        };

        // An extension's installroot is seeded with a copy of the rootfs RPM
        // database so dnf can resolve against the base without reinstalling it.
        // Those rows are not what the extension ships, and a `contains` for
        // them would claim the extension carries the whole base system.
        //
        // The seed is taken once, when the installroot is first created
        // (`ext/install.rs` runs the `cp -rf .../rootfs/var/lib/rpm` setup only
        // `if !sysroot_exists`, and `runtime/install.rs` likewise), so it goes
        // stale as soon as the rootfs moves on.
        //
        // What separates the two is that rpm records an install *transaction*,
        // not a per-package timestamp: every package of one `dnf install`
        // shares one `INSTALLTID`. The seed is a byte copy of the rootfs
        // database, so it arrives as whole transactions, and everything the
        // extension installs afterwards forms transactions of its own. A
        // transaction is therefore the seed exactly when every package in it
        // is one the rootfs also holds.
        //
        // Two rules this replaces, both wrong on real data:
        //
        //   * Subtracting by NVRA drops a package the extension genuinely
        //     installed whenever the rootfs holds the same version — on a real
        //     project, 57 of the 113 packages a runtime installed, because
        //     curl's and vim's dependency closure overlaps the base.
        //
        //   * Matching a row against the rootfs by transaction id assumes the
        //     rootfs still carries the ids the seed was copied from. `install
        //     --force` reinstalls the rootfs in one new transaction, which
        //     replaces all of them at once — so nothing matched, and every
        //     extension reported the whole 139-package base system as its own
        //     content. `--output json` turns `--force` on by itself, so that
        //     was the default in CI.
        //
        // Matching whole transactions survives both: the seed keeps its own
        // ids whatever the rootfs does, and a transaction that installed
        // anything the rootfs lacks is kept entire, overlap included.
        //
        // Membership is by name and architecture, not by version. A rootfs
        // that upgrades one package leaves the extension holding a stale copy
        // of it; on version, that one row would make its whole transaction —
        // the entire seeded base — look like the extension's own content, and
        // a single upgraded package would put 139 packages back into every
        // extension. The name survives an upgrade, so the seed stays
        // recognisable.
        //
        // What this cannot see: a transaction in which the extension installed
        // *only* packages the rootfs already carries by name. It reads as
        // seed, and the extension loses its `contains` for them. That is the
        // benign direction — the packages are still in the document, under the
        // rootfs — and it needs an extension that adds nothing the base lacks,
        // which is an extension with no reason to exist. The opposite error
        // hands every extension the whole base system.
        let base_names: BTreeSet<(String, String)> = dumps
            .iter()
            .find(|d| d.scope == "rootfs")
            .map(|d| {
                to_packages(&d.rows)
                    .into_iter()
                    .map(|p| (p.name, p.arch))
                    .collect()
            })
            .unwrap_or_default();

        dumps
            .into_iter()
            .filter(|d| self.include_sdk || !BUILD_HOST_SCOPES.contains(&d.scope.as_str()))
            .map(|d| {
                let seeded = is_seeded_scope(&d.scope);
                let mut packages = to_packages(&d.rows);
                // Counted before the seeded filter, which drops rows on
                // purpose. This counts only the ones the mapping could not
                // read.
                let unreadable = d.rows.len().saturating_sub(packages.len());
                if seeded && !base_names.is_empty() {
                    // The id is compared as rpm printed it. Grouping only needs
                    // rows of one transaction to carry the same string, so an
                    // id that is not a number still groups with its own copies
                    // instead of collapsing every such row together.
                    let mut all_in_base: BTreeMap<String, bool> = BTreeMap::new();
                    for p in &packages {
                        let entry = all_in_base
                            .entry(p.installtid.trim().to_string())
                            .or_insert(true);
                        *entry &= base_names.contains(&(p.name.clone(), p.arch.clone()));
                    }
                    let seed_tids: BTreeSet<String> = all_in_base
                        .into_iter()
                        .filter(|(_, all)| *all)
                        .map(|(tid, _)| tid)
                        .collect();

                    packages.retain(|p| !seed_tids.contains(p.installtid.trim()));
                }
                Scope {
                    name: d.scope,
                    root: d.root,
                    packages,
                    failed: d.failed,
                    unreadable,
                }
            })
            .collect()
    }

    /// A digest of what the document describes, used to make its element IRIs
    /// unique to this device rather than shared by every device on the target.
    ///
    /// Without it the namespace is a pure function of the target, so two
    /// devices both running `qemuarm64` with different package sets emit
    /// documents whose `SpdxDocument`, `software_Sbom` and scope elements all
    /// carry the same `spdxId`. Ingested into one graph they merge, and one
    /// element ends up with both devices' `contains` sets — device A's packages
    /// attributed to device B. Yocto's own `create-spdx-3.0` puts a unique
    /// value in the namespace for the same reason.
    ///
    /// Derived from the content rather than randomly so the document stays
    /// byte-stable: the same installed set must produce the same bytes twice
    /// running, or a consumer diffing two SBOMs sees churn that is not there.
    /// Two devices holding genuinely identical software share a namespace,
    /// which is correct — the documents are then identical too.
    fn namespace_digest(scopes: &[Scope]) -> String {
        let mut hasher = Sha256::new();
        // Sorted, so the digest does not depend on the order the sysroots were
        // discovered in.
        let mut lines: Vec<String> = Vec::new();
        for scope in scopes.iter().filter(|s| !s.packages.is_empty()) {
            for pkg in &scope.packages {
                let (name, epoch, version, release, arch) = pkg.key();
                lines.push(format!(
                    "{}\t{name}\t{epoch}\t{version}\t{release}\t{arch}",
                    scope.name
                ));
            }
        }
        lines.sort();
        lines.dedup();
        for line in lines {
            hasher.update(line.as_bytes());
            hasher.update(b"\n");
        }
        let digest = hasher.finalize();
        digest[..8].iter().map(|b| format!("{b:02x}")).collect()
    }

    fn build_document(
        &self,
        scopes: &[Scope],
        target: &str,
        snapshot: Option<&RepoSnapshot>,
    ) -> serde_json::Value {
        let ns = format!(
            "https://avocadolinux.org/spdx/{}/{}",
            slug_id(target),
            Self::namespace_digest(scopes)
        );
        let created = created_timestamp(std::env::var("SOURCE_DATE_EPOCH").ok().as_deref());

        let creation_id = format!("{ns}/creationinfo/1");
        let agent_id = format!("{ns}/agent/avocado");
        let tool_id = format!("{ns}/tool/avocado-cli");

        let mut graph: Vec<serde_json::Value> = vec![
            // Given an id and referenced rather than inlined into every
            // element. CreationInfo never varies within a document, and
            // inlining it costs a quarter of the file.
            serde_json::json!({
                "@id": creation_id,
                "type": "CreationInfo",
                "specVersion": SPEC_VERSION,
                "created": created,
                "createdBy": [agent_id],
                "createdUsing": [tool_id],
            }),
            serde_json::json!({
                "type": "Agent", "spdxId": agent_id,
                "name": "Avocado Linux", "creationInfo": creation_id,
            }),
            serde_json::json!({
                "type": "Tool", "spdxId": tool_id,
                "name": concat!("avocado-cli ", env!("CARGO_PKG_VERSION")),
                "creationInfo": creation_id,
            }),
        ];

        // BTreeMap for de-duplication, and so the `element`, `license` and
        // `supplier` arrays below come out sorted rather than in encounter
        // order.
        //
        // It does not order `@graph` itself: package nodes are pushed as the
        // scopes are walked, so their order is rpm's. Two runs over an
        // unchanged project still produce the same bytes — `rpm -qa` walks an
        // unchanged database the same way each time — but that is rpm's
        // property, not this map's, and the stability test replays a fixed
        // dump so it cannot see the difference.
        let mut emitted: BTreeMap<(String, String, String, String, String), String> =
            BTreeMap::new();
        let mut licenses: BTreeMap<String, String> = BTreeMap::new();
        let mut suppliers: BTreeMap<String, String> = BTreeMap::new();
        let mut scope_ids: Vec<(String, String)> = Vec::new();

        // The licence of the document, which is nothing to do with the licences
        // of what it describes. SPDX 3.0 gives `dataLicense` a range of
        // `AnyLicenseInfo`, so it takes an element reference rather than a
        // string: pointing it straight at `https://spdx.org/licenses/CC0-1.0`
        // would name a node this graph never defines, and the shapes check the
        // class of whatever it resolves to. Seeded into the shared license map
        // so a CC0-licensed package reuses this element instead of minting a
        // second one carrying the same expression.
        let data_license_id = format!("{ns}/license/{}", slug_id("CC0-1.0"));
        licenses.insert("CC0-1.0".to_string(), data_license_id.clone());
        graph.push(serde_json::json!({
            "type": "simplelicensing_LicenseExpression",
            "spdxId": data_license_id,
            "creationInfo": creation_id,
            "simplelicensing_licenseExpression": "CC0-1.0",
        }));

        // A scope holding no package is left out of the document entirely, root
        // and all. The discovery walk globs `includes/*/`, which matches plain
        // directories (`includes/etc`, `includes/var`) as readily as a
        // legacy-layout extension's installroot, and a root asserting that
        // `includes:etc` exists and contains nothing describes a directory as
        // if it were a shipped artifact. The human summary still lists them, so
        // "scanned and empty" stays visible where it belongs.
        for scope in scopes.iter().filter(|s| !s.packages.is_empty()) {
            let scope_id = format!("{ns}/scope/{}", slug_id(&scope.name));
            graph.push(serde_json::json!({
                "type": "software_Package",
                "spdxId": scope_id,
                "creationInfo": creation_id,
                "name": scope.name,
                "software_primaryPurpose": "archive",
                "comment": format!("avocado sysroot at {}", scope.root),
            }));
            scope_ids.push((scope.name.clone(), scope_id.clone()));

            let mut members: Vec<String> = Vec::new();
            for pkg in &scope.packages {
                let (name, epoch, version, release, arch) = pkg.key();
                let key = (
                    name.to_string(),
                    epoch.to_string(),
                    version.to_string(),
                    release.to_string(),
                    arch.to_string(),
                );
                let id = emitted.entry(key).or_insert_with(|| {
                    // Slugged as one string rather than per component: slugging
                    // the parts and joining them lets a dash inside a name
                    // trade places with the separator, so `a-b` at version `c`
                    // and `a` at version `b-c` would land on the same id.
                    let id = format!(
                        "{ns}/package/{}",
                        slug_id(&format!("{}-{}.{}", pkg.name, pkg.evr(), pkg.arch))
                    );
                    emit_package(
                        &mut graph,
                        &ns,
                        &creation_id,
                        &id,
                        pkg,
                        &mut licenses,
                        &mut suppliers,
                    );
                    id
                });
                members.push(id.clone());
            }
            members.sort();
            members.dedup();

            // Non-empty by construction — the loop skips empty scopes — which
            // matters because `Relationship.to` is min_count=1 and the SPDX
            // SHACL shapes reject an empty one.
            graph.push(serde_json::json!({
                "type": "Relationship",
                "spdxId": format!("{ns}/rel/contains/{}", slug_id(&scope.name)),
                "creationInfo": creation_id,
                "from": scope_id,
                "relationshipType": "contains",
                "to": members,
            }));
        }

        // An extension is not a peer of the runtime carrying it. The project
        // declares it under that runtime and the runtime is what ships, so a
        // flat list of roots throws away the composition the user wrote — from
        // the document alone there is no way back to which runtime a given
        // extension belongs to.
        //
        // Two scopes keep their root: a legacy `ext:<name>`, whose name carries
        // no runtime to attach to, and an extension whose runtime installed no
        // packages of its own, since that runtime has no element here to hang
        // from. Both are described rather than dropped.
        let by_name: BTreeMap<&str, &str> = scope_ids
            .iter()
            .map(|(n, id)| (n.as_str(), id.as_str()))
            .collect();
        let parent_of = |name: &str| -> Option<&str> {
            // Only `ext:<runtime>/<name>` names a runtime; a legacy `ext:<name>`
            // has no separator and falls out here.
            let (runtime, _) = name.strip_prefix("ext:")?.split_once('/')?;
            by_name.get(format!("runtime:{runtime}").as_str()).copied()
        };

        let mut children: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        let mut roots: Vec<String> = Vec::new();
        for (name, id) in &scope_ids {
            match parent_of(name) {
                Some(parent) => children.entry(parent).or_default().push(id.clone()),
                None => roots.push(id.clone()),
            }
        }
        for (parent, mut kids) in children {
            kids.sort();
            graph.push(serde_json::json!({
                "type": "Relationship",
                "spdxId": format!(
                    "{ns}/rel/contains-extensions/{}",
                    parent.rsplit('/').next().unwrap_or(parent)
                ),
                "creationInfo": creation_id,
                "from": parent,
                "relationshipType": "contains",
                "to": kids,
            }));
        }

        let sbom_id = format!("{ns}/sbom");
        let mut sbom_element = serde_json::json!({
            "type": "software_Sbom",
            "spdxId": sbom_id,
            "creationInfo": creation_id,
            "name": format!("avocado {target} device SBOM"),
            // "deployed", not "build". A Yocto build emits SPDX for the same
            // packages under `build`; this describes what was installed, which
            // is the whole reason the document exists.
            "software_sbomType": ["deployed"],
            "rootElement": roots,
            // Every scope belongs in `element`, roots and extensions alike. A
            // consumer that enumerates the collection through `element` — the
            // one property that says what the collection holds — would
            // otherwise never reach the per-scope packages, and so never reach
            // the `contains` relationships that carry which sysroot holds
            // what. That placement is the entire reason this is one document
            // rather than five. An extension is no longer a root once its
            // runtime carries it, so listing only the roots here would leave
            // the runtime's `contains` pointing outside the collection.
            "element": emitted
                .values()
                .cloned()
                .chain(scope_ids.iter().map(|(_, id)| id.clone()))
                .collect::<Vec<_>>(),
        });

        // Where the packages came from. Without it the document is unjoinable:
        // nothing in it says which feed, channel, or immutable snapshot
        // produced these RPMs, so it cannot be tied back to the build's own
        // SPDX documents, matched against an advisory feed, or re-resolved.
        //
        // Only the lockfile's pinned snapshot is used. The config's declared
        // release and channel say what was asked for rather than what was
        // resolved, and a channel head moves — recording those as provenance
        // would be a claim this document cannot support.
        if let Some(snap) = snapshot {
            let el = sbom_element
                .as_object_mut()
                .expect("a json! object literal is an object");
            let minted = snap
                .created
                .as_deref()
                .map(|c| format!(", minted {c}"))
                .unwrap_or_default();
            el.insert(
                "description".into(),
                serde_json::json!(format!(
                    "Packages resolved from the Avocado {}/{} feed, snapshot {}{}.",
                    snap.release, snap.channel, snap.snapshot, minted
                )),
            );
            if let Some(repo) = &snap.repo_url {
                el.insert(
                    "externalRef".into(),
                    serde_json::json!([{
                        "type": "ExternalRef",
                        "externalRefType": "buildMeta",
                        "locator": [format!(
                            "{}/{}/{}/snapshots/{}/target/{target}/",
                            repo.trim_end_matches('/'),
                            snap.release,
                            snap.channel,
                            snap.snapshot
                        )],
                        "comment": "Immutable feed subtree these packages were resolved from.",
                    }]),
                );
            }
        }
        graph.push(sbom_element);

        graph.push(serde_json::json!({
            "type": "SpdxDocument",
            "spdxId": format!("{ns}/document"),
            "creationInfo": creation_id,
            "name": format!("avocado-{}-sbom", slug(target)),
            "rootElement": [sbom_id],
            "profileConformance": ["core", "software", "simpleLicensing"],
            "dataLicense": data_license_id,
        }));

        serde_json::json!({ "@context": CONTEXT, "@graph": graph })
    }

    /// The summary a `--output json` caller gets on stdout once the document
    /// itself has gone to a file. Same numbers as the human table, in the one
    /// shape that stream promised to carry.
    fn summary_json(
        &self,
        scopes: &[Scope],
        path: Option<&str>,
        packages: usize,
        occurrences: usize,
    ) -> serde_json::Value {
        serde_json::json!({
            "output_path": path,
            "packages": packages,
            "occurrences": occurrences,
            "include_sdk": self.include_sdk,
            "scopes": scopes
                .iter()
                .map(|s| serde_json::json!({ "name": s.name, "packages": s.packages.len() }))
                .collect::<Vec<_>>(),
        })
    }

    fn print_summary(&self, scopes: &[Scope], path: Option<&str>) {
        let mut distinct: BTreeSet<(&str, &str, &str, &str, &str)> = BTreeSet::new();
        for scope in scopes {
            for pkg in &scope.packages {
                distinct.insert(pkg.key());
            }
        }
        let occurrences: usize = scopes.iter().map(|s| s.packages.len()).sum();

        // With `--output json` the document itself has gone to a file, so what
        // stdout owes a caller is the same summary in the one shape it agreed
        // to parse. Printing the human table here would put unparseable lines
        // on a stream a consumer reads as JSON.
        if self.output.is_json() {
            emit_json_object(&self.summary_json(scopes, path, distinct.len(), occurrences));
            return;
        }

        for scope in scopes {
            println!("{:<40} {:>6}", scope.name, scope.packages.len());
        }
        if occurrences > distinct.len() {
            print_info(
                &format!(
                    "{} package occurrence(s) across scopes resolve to {} distinct package(s); \
                     one shared by two sysroots is one SPDX element with two containments.",
                    occurrences,
                    distinct.len()
                ),
                OutputLevel::Normal,
            );
        }
        if !self.include_sdk {
            print_info(
                "The SDK and target sysroot are excluded: they run on the build host and ship \
                 nothing. Pass --include-sdk to audit them too.",
                OutputLevel::Normal,
            );
        }
        if let Some(path) = path {
            print_success(
                &format!("{} package(s) written to {path}.", distinct.len()),
                OutputLevel::Normal,
            );
            // Pointed at rather than done for you, and the offline route
            // named first.
            //
            // The reference SPDX Java Tools are the authority on conformance
            // and the easiest way to reach them is a web service that keeps
            // every upload for about ten days and serves it back without
            // authentication. This document names every package and version on
            // the target, so whether it can be published is the operator's
            // call, made on a document they can read first. Naming only the
            // upload would push every user toward the answer that cannot be
            // taken back.
            print_info(
                &format!(
                    "To check {path} against the SPDX 3.0.1 shapes without it leaving this \
                     machine: `pyshacl -s spdx-model.ttl -e spdx-model.ttl -f human {path}` \
                     (shapes: https://spdx.org/rdf/3.0.1/spdx-model.ttl; the -e is required, or \
                     class constraints pass vacuously). For the reference SPDX tools' verdict, \
                     upload it to https://tools.spdx.org/app/validate/ as JSONLD — but that site \
                     keeps uploads for about ten days and serves them without authentication, so \
                     do not upload a package list that is confidential."
                ),
                OutputLevel::Normal,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_package(
    graph: &mut Vec<serde_json::Value>,
    ns: &str,
    creation_id: &str,
    id: &str,
    pkg: &Package,
    licenses: &mut BTreeMap<String, String>,
    suppliers: &mut BTreeMap<String, String>,
) {
    let mut element = serde_json::json!({
        "type": "software_Package",
        "spdxId": id,
        "creationInfo": creation_id,
        "name": pkg.name,
        "software_packageVersion": pkg.evr(),
        "software_primaryPurpose": "install",
        "software_packageUrl": pkg.purl(),
    });
    let obj = element.as_object_mut().expect("json! built an object");

    // A recipe name is a claim about where the package came from, and rpm
    // writes `(none)` for a row that has no source rpm at all — `gpg-pubkey`,
    // the pseudo-package dnf installs when it imports a repo signing key.
    // "recipe: (none)" reads as a recipe called `(none)`, so the line is left
    // out rather than filled with rpm's placeholder.
    if let Some(recipe) = pkg.recipe() {
        obj.insert(
            "software_attributionText".into(),
            serde_json::json!([format!("recipe: {recipe}")]),
        );
    }

    if let Some(summary) = present(&pkg.summary) {
        obj.insert("summary".into(), summary.into());
    }
    if let Some(url) = present(&pkg.url) {
        obj.insert("software_homePage".into(), url.into());
    }
    if let Some(built) = spdx_time(&pkg.buildtime) {
        obj.insert("builtTime".into(), built.into());
    }
    if let Some(sha) = present(&pkg.sha256) {
        obj.insert(
            "verifiedUsing".into(),
            serde_json::json!([{
                "type": "Hash",
                "algorithm": "sha256",
                "hashValue": sha,
                // The digest of the rpm header, which survives installation.
                // The .rpm file is gone by then, so this is not a checksum of
                // the artifact that was delivered, and a consumer must not
                // treat it as one.
                "comment": "rpm SHA256HEADER, not a digest of the .rpm file",
            }]),
        );
    }
    if let Some(packager) = present(&pkg.packager) {
        // The map is the record of what has been emitted; a miss is the only
        // signal needed. Scanning the graph for the id instead would be a
        // second source of truth, and quadratic in the package count.
        let sup_id = match suppliers.get(packager) {
            Some(id) => id.clone(),
            None => {
                // `/supplier/`, not `/agent/`: the tool's own agent is at
                // `{ns}/agent/avocado`, and `slug_id("avocado")` is `avocado`
                // unchanged, so a package whose PACKAGER is that bare string
                // would land two different Agents on one spdxId.
                let id = format!("{ns}/supplier/{}", slug_id(packager));
                suppliers.insert(packager.to_string(), id.clone());
                graph.push(serde_json::json!({
                    "type": "Agent", "spdxId": id,
                    "name": packager, "creationInfo": creation_id,
                }));
                id
            }
        };
        obj.insert("suppliedBy".into(), sup_id.into());
    }
    graph.push(element);

    // Licenses are shared elements referenced by id rather than repeated: 273
    // packages draw on 31 distinct expressions on a real project.
    let expr = spdx_license(&pkg.license);
    let lic_id = match licenses.get(&expr) {
        Some(id) => id.clone(),
        None => {
            let id = format!("{ns}/license/{}", slug_id(&expr));
            licenses.insert(expr.clone(), id.clone());
            graph.push(serde_json::json!({
                "type": "simplelicensing_LicenseExpression",
                "spdxId": id,
                "creationInfo": creation_id,
                "simplelicensing_licenseExpression": expr,
            }));
            id
        }
    };
    graph.push(serde_json::json!({
        "type": "Relationship",
        "spdxId": format!("{ns}/rel/declared-license/{}", id.rsplit('/').next().unwrap_or(id)),
        "creationInfo": creation_id,
        "from": id,
        "relationshipType": "hasDeclaredLicense",
        "to": [lic_id],
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(include_sdk: bool) -> SbomCommand {
        SbomCommand::new(
            "avocado.yaml".to_string(),
            None,
            None,
            include_sdk,
            false,
            None,
            OutputFormat::Json,
        )
    }

    /// One row in the shape `RPM_SBOM_FORMAT` produces, with no epoch and a
    /// fixed install transaction — the shape a plain rootfs row has.
    fn row(name: &str, version: &str, release: &str, arch: &str, license: &str) -> String {
        row_full(name, version, release, arch, license, "(none)", 1000)
    }

    /// The same row with the two fields that carry identity beyond the NVRA:
    /// the epoch, and the transaction that installed it.
    fn row_full(
        name: &str,
        version: &str,
        release: &str,
        arch: &str,
        license: &str,
        epoch: &str,
        installtid: u64,
    ) -> String {
        format!(
            "{name}\t{version}\t{release}\t{arch}\t{epoch}\t{license}\thttps://example.invalid\t\
             Avocado Developers <info@avocadolinux.org>\t{name}-{version}-{release}.src.rpm\t\
             {}\t1684449060\t{installtid}\tA package\n",
            "b6042ed7a9d91889953b11b0456135d5b1bff1bee22a4ff197076e041dff6c85"
        )
    }

    /// Split a purl back into its parts, the way a consumer would.
    ///
    /// Written here rather than asserted against a fixed string: the point is
    /// that the components survive the round trip, and a literal expectation
    /// would pass just as happily on a purl whose boundaries had moved.
    fn parse_purl(purl: &str) -> (String, String, String, String, Vec<(String, String)>) {
        let rest = purl.strip_prefix("pkg:").expect("purl scheme");
        let (before_qual, qual) = match rest.split_once('?') {
            Some((a, b)) => (a, b),
            None => (rest, ""),
        };
        let (path, version) = match before_qual.rsplit_once('@') {
            Some((a, b)) => (a, b),
            None => (before_qual, ""),
        };
        let mut segments = path.splitn(3, '/');
        let typ = segments.next().unwrap_or_default().to_string();
        let namespace = segments.next().unwrap_or_default().to_string();
        let name = segments.next().unwrap_or_default().to_string();
        let qualifiers = qual
            .split('&')
            .filter(|q| !q.is_empty())
            .map(|q| {
                let (k, v) = q.split_once('=').unwrap_or((q, ""));
                (k.to_string(), v.to_string())
            })
            .collect();
        (typ, namespace, name, version.to_string(), qualifiers)
    }

    fn decode(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
                out.push(u8::from_str_radix(hex, 16).unwrap());
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap()
    }

    /// A Package with only the purl-relevant fields worth setting.
    fn blank() -> Package {
        Package {
            name: String::new(),
            version: String::new(),
            release: String::new(),
            arch: String::new(),
            epoch: "(none)".into(),
            license: "MIT".into(),
            url: "(none)".into(),
            packager: "(none)".into(),
            sourcerpm: "x-1.0-r0.src.rpm".into(),
            sha256: "(none)".into(),
            buildtime: "1684449060".into(),
            installtid: "1700000000".into(),
            summary: "(none)".into(),
        }
    }

    #[test]
    fn a_purl_round_trips_for_the_shapes_a_real_device_holds() {
        // Nothing in the SPDX shapes validates this field, so it needs its own
        // test: a malformed purl passes SHACL and fails silently downstream.
        // These are the awkward shapes on a real qemuarm64 rootfs.
        for (name, version, release, arch) in [
            ("libssl3", "3.5.7", "r0.2", "cortexa57"),
            // `+` in the name: 29 of 273 packages carry one.
            ("libstdc++6", "13.2.0", "r0.2", "cortexa57"),
            // The form Yocto gives a git recipe, and the `-` -> `+` rewrite
            // package_rpm.bbclass applies to a hyphenated PKGV.
            ("ldconfig", "2.39+git0+ce65d944e3", "r0.2", "cortexa57"),
            ("libedit", "20230828+3.1", "r1.2", "cortexa57"),
        ] {
            let p = Package {
                name: name.into(),
                version: version.into(),
                release: release.into(),
                arch: arch.into(),
                ..blank()
            };
            let purl = p.purl();
            let (typ, ns, got_name, got_version, quals) = parse_purl(&purl);
            assert_eq!(typ, "rpm", "{purl}");
            assert_eq!(ns, "avocado", "{purl}");
            assert_eq!(decode(&got_name), name, "{purl}");
            assert_eq!(
                decode(&got_version),
                format!("{version}-{release}"),
                "{purl}"
            );
            assert_eq!(
                quals,
                vec![("arch".to_string(), arch.to_string())],
                "{purl}"
            );
            // `+` is not a purl separator and must survive literally: encoding
            // it would make this purl stop matching the one a scanner builds
            // from the same rpm.
            assert!(!purl.contains("%2B"), "{purl}");
        }
    }

    #[test]
    fn a_component_holding_a_purl_separator_is_encoded() {
        // None of these can come out of rpm today. They are pinned because the
        // failure is silent: a literal `@` in a name moves the version boundary
        // and a parser reads a different package, with nothing to say so.
        for (raw, encoded) in [
            ("a@b", "a%40b"),
            ("a/b", "a%2Fb"),
            ("a?b", "a%3Fb"),
            ("a#b", "a%23b"),
            ("a:b", "a%3Ab"),
            ("a%b", "a%25b"),
            ("a&b", "a%26b"),
            ("a=b", "a%3Db"),
            ("a b", "a%20b"),
        ] {
            assert_eq!(purl_encode(raw), encoded);
        }
        // And the ones that must not move.
        for raw in ["libstdc++6", "2.39+git0+ce65d944e3", "a.b-c_d", "cortexa57"] {
            assert_eq!(purl_encode(raw), raw);
        }
    }

    #[test]
    fn an_encoded_component_still_round_trips() {
        let p = Package {
            name: "weird@name".into(),
            version: "1.0?x".into(),
            release: "r0".into(),
            arch: "noarch".into(),
            ..blank()
        };
        let purl = p.purl();
        let (_, _, name, version, quals) = parse_purl(&purl);
        assert_eq!(decode(&name), "weird@name");
        assert_eq!(decode(&version), "1.0?x-r0");
        assert_eq!(quals, vec![("arch".to_string(), "noarch".to_string())]);
    }

    #[test]
    fn yocto_license_strings_become_spdx_expressions() {
        assert_eq!(spdx_license("LGPL-2.1-or-later"), "LGPL-2.1-or-later");
        assert_eq!(
            spdx_license("GPL-2.0-only & LGPL-2.1-or-later"),
            "GPL-2.0-only AND LGPL-2.1-or-later"
        );
        assert_eq!(spdx_license("MIT | Apache-2.0"), "MIT OR Apache-2.0");
        // Not on the SPDX list, so it has to be declared unresolvable rather
        // than emitted as if a consumer could look it up.
        assert_eq!(
            spdx_license("GPL-2.0-only & bzip2-1.0.4"),
            "GPL-2.0-only AND LicenseRef-bzip2-1.0.4"
        );
        assert_eq!(spdx_license("PD"), "LicenseRef-PD");
        assert_eq!(spdx_license("   "), "NOASSERTION");
        assert_eq!(
            spdx_license("(MIT | ISC) & Zlib"),
            "( MIT OR ISC ) AND Zlib"
        );
    }

    #[test]
    fn the_recipe_survives_a_version_that_holds_dashes() {
        let mut p = pkg("glibc-bin", "2.39+git0+ce65d944e3", "r0.2");
        p.sourcerpm = "glibc-2.39+git0+ce65d944e3-r0.2.src.rpm".to_string();
        assert_eq!(p.recipe(), Some("glibc"));

        // A recipe name with dashes of its own.
        p.sourcerpm = "util-linux-2.39.3-r0.2.src.rpm".to_string();
        assert_eq!(p.recipe(), Some("util-linux"));
    }

    #[test]
    fn a_package_with_no_source_rpm_claims_no_recipe() {
        // rpm writes `(none)` for an unset tag, and `gpg-pubkey` — installed by
        // dnf when it imports a repo signing key — has neither a source rpm nor
        // an arch. Emitted verbatim, the document states a recipe named
        // `(none)` and a purl no scanner can match against the same rpm.
        let mut p = pkg("gpg-pubkey", "3fa7e0328081bff6", "a4d3d5e9");
        p.sourcerpm = "(none)".to_string();
        p.arch = "(none)".to_string();

        assert_eq!(p.recipe(), None);

        let purl = p.purl();
        assert!(
            !purl.contains("(none)") && !purl.contains("arch="),
            "an absent arch is dropped rather than carried as a qualifier that matches nothing; \
             got: {purl}"
        );
        assert!(
            purl.starts_with("pkg:rpm/avocado/gpg-pubkey@3fa7e0328081bff6-a4d3d5e9"),
            "the rest of the purl is unchanged; got: {purl}"
        );

        // The epoch qualifier has to keep its leading `?` now that `arch` is
        // no longer guaranteed to be the first one.
        p.epoch = "2".to_string();
        assert_eq!(
            p.purl(),
            "pkg:rpm/avocado/gpg-pubkey@3fa7e0328081bff6-a4d3d5e9?epoch=2"
        );
    }

    #[test]
    fn a_package_with_no_recipe_carries_no_attribution_line() {
        // The document side of the same row: an absent recipe means the
        // property is left off, the way every other unset tag is, rather than
        // emitted as prose naming a recipe that does not exist.
        let dump = format!(
            "##SCOPE\trootfs\t/rootfs\n{}",
            row(
                "gpg-pubkey",
                "3fa7e0328081bff6",
                "a4d3d5e9",
                "(none)",
                "MIT"
            ),
        );

        let c = cmd(false);
        let mut scopes = c.parse_scopes(&dump);
        scopes[0].packages[0].sourcerpm = "(none)".to_string();

        let doc = c.build_document(&scopes, "qemuarm64", None);
        let pkg = doc["@graph"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == "gpg-pubkey")
            .expect("the package is in the document");

        assert!(pkg.get("software_attributionText").is_none(), "got: {pkg}");
        assert!(
            !pkg["software_packageUrl"]
                .as_str()
                .unwrap()
                .contains("(none)"),
            "got: {pkg}"
        );
    }

    fn pkg(name: &str, version: &str, release: &str) -> Package {
        Package {
            name: name.to_string(),
            version: version.to_string(),
            release: release.to_string(),
            arch: "cortexa57".to_string(),
            epoch: "(none)".to_string(),
            license: "MIT".to_string(),
            url: "(none)".to_string(),
            packager: "(none)".to_string(),
            sourcerpm: format!("{name}-{version}-{release}.src.rpm"),
            sha256: "(none)".to_string(),
            buildtime: "1684449060".to_string(),
            installtid: "1000".to_string(),
            summary: "(none)".to_string(),
        }
    }

    #[test]
    fn rpm_none_and_a_bad_timestamp_leave_the_property_out() {
        assert_eq!(present("(none)"), None);
        assert_eq!(present("  "), None);
        assert_eq!(present(" x "), Some("x"));
        assert_eq!(
            spdx_time("1684449060").as_deref(),
            Some("2023-05-18T22:31:00Z")
        );
        // The spec pins the format, so an unparseable epoch must drop the
        // property rather than emit something the shapes reject.
        assert_eq!(spdx_time("not-a-number"), None);
    }

    #[test]
    fn a_package_in_two_scopes_is_one_element_with_two_containments() {
        // The whole reason this command emits one document rather than one per
        // scope: split across files, rootfs's libc6 and initramfs's libc6 are
        // two SPDX elements, and a scanner counts the same exposure twice.
        let dump = format!(
            "##SCOPE\trootfs\t/opt/_avocado/qemuarm64/rootfs\n{}{}\
             ##SCOPE\tinitramfs\t/opt/_avocado/qemuarm64/initramfs\n{}",
            row("libc6", "2.39", "r0.2", "cortexa57", "LGPL-2.1-or-later"),
            row("busybox", "1.36.1", "r0.2", "cortexa57", "GPL-2.0-only"),
            row("libc6", "2.39", "r0.2", "cortexa57", "LGPL-2.1-or-later"),
        );

        let c = cmd(false);
        let scopes = c.parse_scopes(&dump);
        assert_eq!(scopes.len(), 2);

        let doc = c.build_document(&scopes, "qemuarm64", None);
        let graph = doc["@graph"].as_array().unwrap();

        let packages: Vec<&str> = graph
            .iter()
            .filter(|e| e["type"] == "software_Package")
            .filter_map(|e| e["name"].as_str())
            .collect();
        // Two real packages plus the two scope roots, and libc6 exactly once.
        assert_eq!(packages.iter().filter(|n| **n == "libc6").count(), 1);

        let contains: Vec<&serde_json::Value> = graph
            .iter()
            .filter(|e| e["relationshipType"] == "contains")
            .collect();
        assert_eq!(contains.len(), 2);
        let libc_id = graph
            .iter()
            .find(|e| e["name"] == "libc6")
            .and_then(|e| e["spdxId"].as_str())
            .unwrap();
        assert_eq!(
            contains
                .iter()
                .filter(|r| r["to"].as_array().unwrap().iter().any(|t| t == libc_id))
                .count(),
            2,
            "one element, contained by both scopes"
        );

        let sbom = graph.iter().find(|e| e["type"] == "software_Sbom").unwrap();
        assert_eq!(sbom["software_sbomType"][0], "deployed");
        // The two distinct packages and the two scope roots: libc6 is one
        // element however many scopes hold it, and the roots are members of
        // the collection they root.
        assert_eq!(sbom["element"].as_array().unwrap().len(), 4);
        assert_eq!(sbom["rootElement"].as_array().unwrap().len(), 2);
    }

    /// Every `spdxId` in the graph carrying the given element name.
    fn ids_named(graph: &[serde_json::Value], name: &str) -> Vec<String> {
        graph
            .iter()
            .filter(|e| e["name"] == name)
            .filter_map(|e| e["spdxId"].as_str())
            .map(str::to_string)
            .collect()
    }

    fn strings(v: &serde_json::Value) -> Vec<&str> {
        v.as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn an_extension_hangs_from_the_runtime_that_carries_it() {
        // The project declares an extension under a runtime and the runtime is
        // what ships. Left as sibling roots, the document cannot say which
        // runtime a given extension belongs to.
        let dump = format!(
            "##SCOPE\trootfs\t/rootfs\n{}\
             ##SCOPE\truntime:dev\t/runtimes/dev\n{}\
             ##SCOPE\text:dev/app\t/runtimes/dev/extensions/app\n{}",
            row_full("libc6", "2.39", "r0.2", "cortexa57", "MIT", "(none)", 1000),
            row_full(
                "avocado-runtime",
                "0.0.0",
                "r0.0",
                "cortexa57",
                "MIT",
                "(none)",
                2000
            ),
            row_full("curl", "8.7.1", "r0.2", "cortexa57", "MIT", "(none)", 3000),
        );

        let c = cmd(false);
        let doc = c.build_document(&c.parse_scopes(&dump), "qemuarm64", None);
        let graph = doc["@graph"].as_array().unwrap();

        let runtime = ids_named(graph, "runtime:dev").remove(0);
        let ext = ids_named(graph, "ext:dev/app").remove(0);

        let sbom = graph.iter().find(|e| e["type"] == "software_Sbom").unwrap();
        let roots = strings(&sbom["rootElement"]);
        assert!(roots.contains(&runtime.as_str()));
        assert!(
            !roots.contains(&ext.as_str()),
            "an extension carried by a runtime is not a peer of it"
        );

        // Still a member of the collection: the containment below would
        // otherwise point at an element the SBOM does not hold.
        assert!(strings(&sbom["element"]).contains(&ext.as_str()));

        assert!(
            graph.iter().any(|e| {
                e["relationshipType"] == "contains"
                    && e["from"] == runtime.as_str()
                    && strings(&e["to"]).contains(&ext.as_str())
            }),
            "the runtime contains its extension"
        );
    }

    #[test]
    fn a_legacy_extension_naming_no_runtime_stays_a_root() {
        // The discovery walk still matches a flat `extensions/<name>` layout,
        // whose scope name carries no runtime to attach to. Describing it as a
        // root beats guessing at a parent.
        let dump = format!(
            "##SCOPE\trootfs\t/rootfs\n{}\
             ##SCOPE\text:app\t/extensions/app\n{}",
            row_full("libc6", "2.39", "r0.2", "cortexa57", "MIT", "(none)", 1000),
            row_full("curl", "8.7.1", "r0.2", "cortexa57", "MIT", "(none)", 3000),
        );

        let c = cmd(false);
        let doc = c.build_document(&c.parse_scopes(&dump), "qemuarm64", None);
        let graph = doc["@graph"].as_array().unwrap();
        let ext = ids_named(graph, "ext:app").remove(0);

        let sbom = graph.iter().find(|e| e["type"] == "software_Sbom").unwrap();
        assert!(strings(&sbom["rootElement"]).contains(&ext.as_str()));
    }

    #[test]
    fn provenance_names_the_snapshot_the_packages_were_resolved_from() {
        let dump = format!(
            "##SCOPE\trootfs\t/rootfs\n{}",
            row("libc6", "2.39", "r0.2", "cortexa57", "MIT")
        );
        let c = cmd(false);
        let snap = RepoSnapshot {
            release: "2024".into(),
            channel: "edge".into(),
            snapshot: "5".into(),
            // Trailing slash on purpose: it is a configured value, and the
            // locator must not come out with a doubled separator.
            repo_url: Some("https://repo.avocadolinux.org/".into()),
            created: Some("2026-07-08T02:17:53Z".into()),
        };

        let doc = c.build_document(&c.parse_scopes(&dump), "qemuarm64", Some(&snap));
        let graph = doc["@graph"].as_array().unwrap();
        let sbom = graph.iter().find(|e| e["type"] == "software_Sbom").unwrap();

        let description = sbom["description"].as_str().unwrap();
        assert!(description.contains("2024/edge"), "{description}");
        assert!(description.contains("snapshot 5"), "{description}");

        assert_eq!(sbom["externalRef"][0]["externalRefType"], "buildMeta");
        assert_eq!(
            sbom["externalRef"][0]["locator"][0],
            "https://repo.avocadolinux.org/2024/edge/snapshots/5/target/qemuarm64/"
        );

        // Unpinned, the document says nothing rather than repeating the
        // release and channel the config asked for: the channel head moves, so
        // that would be provenance the document cannot stand behind.
        let bare = c.build_document(&c.parse_scopes(&dump), "qemuarm64", None);
        let bare = bare["@graph"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["type"] == "software_Sbom")
            .unwrap()
            .clone();
        assert!(bare.get("description").is_none());
        assert!(bare.get("externalRef").is_none());
    }

    #[test]
    fn the_document_licenses_itself_with_an_element_not_a_url() {
        // `dataLicense` ranges over AnyLicenseInfo, so pointing it at the bare
        // license-list URL would name a node this graph never defines and the
        // shapes would have nothing to check the class of.
        let dump = format!(
            "##SCOPE\trootfs\t/rootfs\n{}{}",
            row("libc6", "2.39", "r0.2", "cortexa57", "MIT"),
            // A CC0 package shares the document's own license element rather
            // than minting a second one carrying the same expression.
            row("public-domain-thing", "1.0", "r0.0", "cortexa57", "CC0-1.0"),
        );

        let c = cmd(false);
        let doc = c.build_document(&c.parse_scopes(&dump), "qemuarm64", None);
        let graph = doc["@graph"].as_array().unwrap();

        let spdx_doc = graph.iter().find(|e| e["type"] == "SpdxDocument").unwrap();
        let license_id = spdx_doc["dataLicense"].as_str().unwrap();

        let cc0: Vec<&serde_json::Value> = graph
            .iter()
            .filter(|e| e["simplelicensing_licenseExpression"] == "CC0-1.0")
            .collect();
        assert_eq!(cc0.len(), 1, "one element for one expression");
        assert_eq!(cc0[0]["type"], "simplelicensing_LicenseExpression");
        assert_eq!(cc0[0]["spdxId"], license_id);
    }

    #[test]
    fn source_date_epoch_pins_the_timestamp_that_would_otherwise_be_now() {
        // The one field that varies between two runs over an unchanged
        // sysroot. Pinned, the whole document is byte-stable, which is what
        // the content-derived namespace exists to deliver.
        assert_eq!(
            created_timestamp(Some("1720404000")),
            "2024-07-08T02:00:00Z"
        );
        // Whitespace survives the shell and the exporting tool.
        assert_eq!(created_timestamp(Some(" 0 ")), "1970-01-01T00:00:00Z");

        // Anything unusable falls back to now rather than to a fixed epoch: a
        // typo silently dating every document to 1970 is worse than a document
        // that is honestly not reproducible.
        for bad in [None, Some(""), Some("yesterday"), Some("1.5e9")] {
            assert_ne!(created_timestamp(bad), "1970-01-01T00:00:00Z");
        }
    }

    #[test]
    fn a_seeded_scope_does_not_claim_the_rootfs_database_it_was_given() {
        // ext/install.rs copies $AVOCADO_PREFIX/rootfs/var/lib/rpm into the
        // extension's installroot so dnf can resolve against the base. Listing
        // those as the extension's contents would say every extension ships the
        // whole base system.
        //
        // `myapp` carries a later transaction than the seed because that is
        // what it would have: the copy happens when the installroot is
        // created, and anything the extension installs is a transaction after
        // it.
        let dump = format!(
            "##SCOPE\trootfs\t/rootfs\n{}\
             ##SCOPE\text:dev/app\t/runtimes/dev/extensions/app\n{}{}",
            row_full("libc6", "2.39", "r0.2", "cortexa57", "MIT", "(none)", 1000),
            row_full("libc6", "2.39", "r0.2", "cortexa57", "MIT", "(none)", 1000),
            row_full("myapp", "1.0", "r0.0", "cortexa57", "MIT", "(none)", 2000),
        );

        let scopes = cmd(false).parse_scopes(&dump);
        let ext = scopes.iter().find(|s| s.name == "ext:dev/app").unwrap();
        assert_eq!(ext.packages.len(), 1);
        assert_eq!(ext.packages[0].name, "myapp");
    }

    #[test]
    fn the_json_summary_reports_every_scope_including_the_empty_ones() {
        // With `--output json -o file` the document goes to the file and stdout
        // carries the summary. The human path prints a table there; on a stream
        // a consumer parses as JSON those lines are garbage, so the same
        // numbers have to leave as one object — scopes that contributed nothing
        // included, since "scanned and empty" and "not scanned" differ.
        let dump = format!(
            "##SCOPE\trootfs\t/rootfs\n{}{}\
             ##SCOPE\tincludes:etc\t/includes/etc\n",
            row("libc6", "2.39", "r0.2", "cortexa57", "MIT"),
            row("busybox", "1.36.1", "r0.2", "cortexa57", "GPL-2.0-only"),
        );

        let c = cmd(false);
        let scopes = c.parse_scopes(&dump);
        let summary = c.summary_json(&scopes, Some("sbom.json"), 2, 2);

        assert_eq!(summary["output_path"], "sbom.json");
        assert_eq!(summary["packages"], 2);
        assert_eq!(summary["occurrences"], 2);
        assert_eq!(summary["include_sdk"], false);

        let by_scope: Vec<(&str, u64)> = summary["scopes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (s["name"].as_str().unwrap(), s["packages"].as_u64().unwrap()))
            .collect();
        assert_eq!(by_scope, vec![("rootfs", 2), ("includes:etc", 0)]);
    }

    #[test]
    fn an_empty_scope_is_kept_out_of_the_document() {
        // The discovery walk globs includes/*/, which matches plain directories
        // (includes/etc, includes/var) as readily as an extension installroot.
        // A root asserting includes:etc exists and contains nothing describes a
        // directory as a shipped artifact — and an empty `contains` is rejected
        // by the SPDX shapes outright, since Relationship.to is min_count=1.
        let dump = format!(
            "##SCOPE\trootfs\t/rootfs\n{}\
             ##SCOPE\tincludes:etc\t/includes/etc\n",
            row("libc6", "2.39", "r0.2", "cortexa57", "MIT"),
        );

        let c = cmd(false);
        let scopes = c.parse_scopes(&dump);
        assert_eq!(scopes.len(), 2, "still scanned, and still summarised");

        let doc = c.build_document(&scopes, "qemuarm64", None);
        let graph = doc["@graph"].as_array().unwrap();
        assert!(!graph.iter().any(|e| e["name"] == "includes:etc"));
        assert!(graph.iter().all(
            |e| e["relationshipType"] != "contains" || !e["to"].as_array().unwrap().is_empty()
        ));
    }

    #[test]
    fn the_sdk_is_excluded_unless_asked_for() {
        let dump = format!(
            "##SCOPE\tsdk\t/opt/_avocado/sdk\n{}\
             ##SCOPE\trootfs\t/rootfs\n{}",
            row("nativesdk-curl", "8.7", "r0.0", "x86_64", "MIT"),
            row("libc6", "2.39", "r0.2", "cortexa57", "MIT"),
        );

        let without = cmd(false).parse_scopes(&dump);
        assert!(!without.iter().any(|s| s.name == "sdk"));

        let with = cmd(true).parse_scopes(&dump);
        assert!(with.iter().any(|s| s.name == "sdk"));
    }

    #[test]
    fn ids_distinguish_strings_the_slug_alone_would_merge() {
        // `slug` maps every non-alphanumeric run to one `-`, so `GPL-2.0+` and
        // `GPL-2.0` collapse together. Two graph nodes at one spdxId carrying
        // different content means a consumer resolving `hasDeclaredLicense`
        // gets whichever it read last: a package under the wrong license.
        assert_eq!(slug("GPL-2.0+"), slug("GPL-2.0"), "the hazard being fixed");
        assert_ne!(slug_id("GPL-2.0+"), slug_id("GPL-2.0"));

        // The common case stays readable rather than being hashed wholesale.
        assert_eq!(slug_id("GPL-2.0"), "GPL-2.0");
        assert_eq!(
            slug_id("busybox-1.36.1-r0.2.cortexa57"),
            "busybox-1.36.1-r0.2.cortexa57"
        );

        // Still a usable id when nothing of the input survives slugging.
        assert!(!slug_id("+++").is_empty());
        assert_ne!(slug_id("+++"), slug_id("///"));
    }

    #[test]
    fn two_devices_on_one_target_do_not_share_element_ids() {
        // The namespace used to be a pure function of the target, so every
        // qemuarm64 document reused one set of IRIs. Ingest two devices into
        // one graph and the elements merge — device A's packages land under
        // device B's `contains`.
        let a = format!(
            "##SCOPE\trootfs\t/rootfs\n{}",
            row("libc6", "2.39", "r0.2", "cortexa57", "MIT")
        );
        let b = format!(
            "##SCOPE\trootfs\t/rootfs\n{}{}",
            row("libc6", "2.39", "r0.2", "cortexa57", "MIT"),
            row("openssh", "9.6", "r0.1", "cortexa57", "BSD-2-Clause"),
        );

        let c = cmd(false);
        let doc_a = c.build_document(&c.parse_scopes(&a), "qemuarm64", None);
        let doc_b = c.build_document(&c.parse_scopes(&b), "qemuarm64", None);

        let id = |d: &serde_json::Value| {
            d["@graph"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["type"] == "software_Sbom")
                .unwrap()["spdxId"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_ne!(id(&doc_a), id(&doc_b));

        // Same input, same IRIs — the byte-stability guarantee has to survive
        // the namespace becoming content-derived.
        assert_eq!(
            id(&doc_a),
            id(&c.build_document(&c.parse_scopes(&a), "qemuarm64", None))
        );
    }

    #[test]
    fn a_drifted_rootfs_does_not_turn_seeded_rows_into_extension_content() {
        // The seed is copied once, when the installroot is created, so it goes
        // stale as soon as the rootfs upgrades. Matching by NVRA against the
        // current rootfs then reports the extension's stale copy as content it
        // ships — a package that is not on the device at all.
        //
        // A stale row matches no rootfs row at all, so what dates it is the
        // rest of the seed: `busybox` below is still identical on both sides,
        // which places the copy at transaction 1000 and makes the stale
        // `libc6` older than the extension itself. That is the real shape — a
        // seeded installroot holds the whole base, of which one package
        // drifted.
        let dump = format!(
            "##SCOPE\trootfs\t/rootfs\n{}{}\
             ##SCOPE\text:dev/app\t/runtimes/dev/extensions/app\n{}{}{}",
            // rootfs upgraded libc6 in a later transaction than the seed
            row_full("libc6", "2.39", "r0.3", "cortexa57", "MIT", "(none)", 2000),
            row_full(
                "busybox",
                "1.36.1",
                "r0.2",
                "cortexa57",
                "GPL-2.0-only",
                "(none)",
                1000
            ),
            // the extension's installroot still holds the pre-upgrade copy
            row_full("libc6", "2.39", "r0.2", "cortexa57", "MIT", "(none)", 1000),
            row_full(
                "busybox",
                "1.36.1",
                "r0.2",
                "cortexa57",
                "GPL-2.0-only",
                "(none)",
                1000
            ),
            row_full("myapp", "1.0", "r0.0", "cortexa57", "MIT", "(none)", 3000),
        );

        let scopes = cmd(false).parse_scopes(&dump);
        let ext = scopes.iter().find(|s| s.name == "ext:dev/app").unwrap();
        let names: Vec<&str> = ext.packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["myapp"], "stale seed row must not be claimed");
    }

    #[test]
    fn an_extension_installing_a_package_the_rootfs_also_has_keeps_it() {
        // The converse failure of NVRA subtraction: a package the extension
        // genuinely installs at the rootfs's own version was dropped from its
        // scope entirely. It is kept because it arrived in a transaction that
        // also installed something the rootfs does not have — which is what an
        // extension installing a dependency looks like.
        let dump = format!(
            "##SCOPE\trootfs\t/rootfs\n{}\
             ##SCOPE\text:dev/app\t/runtimes/dev/extensions/app\n{}{}",
            row_full("curl", "8.7", "r0.1", "cortexa57", "MIT", "(none)", 1000),
            row_full("curl", "8.7", "r0.1", "cortexa57", "MIT", "(none)", 4000),
            row_full("myapp", "1.0", "r0.0", "cortexa57", "MIT", "(none)", 4000),
        );

        let scopes = cmd(false).parse_scopes(&dump);
        let ext = scopes.iter().find(|s| s.name == "ext:dev/app").unwrap();
        let mut names: Vec<&str> = ext.packages.iter().map(|p| p.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["curl", "myapp"]);
    }

    #[test]
    fn an_extension_that_adds_nothing_new_is_read_as_seed_and_that_is_the_known_limit() {
        // Pinned deliberately, because it is the price of matching whole
        // transactions.
        //
        // A transaction holding only packages the rootfs already carries by
        // name is indistinguishable from a copy of the rootfs database, so it
        // reads as seed and the extension loses its `contains` for them. They
        // stay in the document under the rootfs, so the inventory is complete
        // — only the placement is missed.
        //
        // The opposite error is worse in the direction that matters: reading
        // such a transaction as content means one package the rootfs upgraded
        // puts the entire seeded base back into every extension.
        let dump = format!(
            "##SCOPE\trootfs\t/rootfs\n{}\
             ##SCOPE\text:dev/app\t/runtimes/dev/extensions/app\n{}",
            row_full("curl", "8.7", "r0.1", "cortexa57", "MIT", "(none)", 1000),
            // installed by the extension, in its own later transaction, with
            // nothing alongside it that the rootfs lacks
            row_full("curl", "8.7", "r0.1", "cortexa57", "MIT", "(none)", 4000),
        );

        let c = cmd(false);
        let scopes = c.parse_scopes(&dump);
        let ext = scopes.iter().find(|s| s.name == "ext:dev/app").unwrap();
        assert!(ext.packages.is_empty(), "the known limitation");

        // Still in the inventory, under the rootfs.
        let doc = c.build_document(&scopes, "qemuarm64", None);
        assert!(doc["@graph"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["type"] == "software_Package" && e["name"] == "curl"));
    }

    #[test]
    fn a_forced_reinstall_of_the_rootfs_does_not_hand_every_extension_the_base_system() {
        // From a real project, with the transaction ids it actually produced.
        // `avocado install -f` twice: the extension installroots were seeded
        // during the first run (1786562080) and left alone by the second,
        // which reinstalled the rootfs in one transaction (1786562364) and so
        // replaced every id the seed had been copied from.
        //
        // Matching seeded rows *by* transaction id therefore matched nothing,
        // and each extension claimed the whole 139-package base as its own.
        // Whole transactions survive it: the seed keeps its own id whatever
        // the rootfs does. `--output json` enables --force by itself, so this
        // was the default in CI.
        const BASE: u64 = 1786562364;
        const SEED: u64 = 1786562080;
        const OWN: u64 = 1786562118;

        let base_pkgs = ["libc6", "busybox", "openssl"];
        let rootfs: String = base_pkgs
            .iter()
            .map(|n| row_full(n, "2.39", "r0.2", "cortexa57", "MIT", "(none)", BASE))
            .collect();
        // The seed is a byte copy: same NVRA, and the id it was copied with.
        let seeded: String = base_pkgs
            .iter()
            .map(|n| row_full(n, "2.39", "r0.2", "cortexa57", "MIT", "(none)", SEED))
            .collect();

        let dump = format!(
            "##SCOPE\trootfs\t/rootfs\n{rootfs}\
             ##SCOPE\text:dev/app\t/runtimes/dev/extensions/app\n{seeded}{}",
            row_full("myapp", "1.0", "r0.0", "cortexa57", "MIT", "(none)", OWN),
        );

        let scopes = cmd(false).parse_scopes(&dump);
        let ext = scopes.iter().find(|s| s.name == "ext:dev/app").unwrap();
        let names: Vec<&str> = ext.packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["myapp"],
            "the seed transaction must not survive"
        );
    }

    #[test]
    fn a_transaction_that_installed_anything_new_is_kept_whole() {
        // The other half, and why subtracting by NVRA is not enough: a runtime
        // installing curl and vim pulls in dependencies the rootfs already
        // has. On the real project that was 57 of 113 packages — dropping them
        // would take more than half of what the runtime installed out of its
        // scope. The transaction installed something new, so all of it is the
        // runtime's.
        const BASE: u64 = 1786562364;
        const OWN: u64 = 1786562421;

        let dump = format!(
            "##SCOPE\trootfs\t/rootfs\n{}{}\
             ##SCOPE\truntime:dev\t/runtimes/dev\n{}{}",
            row_full("libc6", "2.39", "r0.2", "cortexa57", "MIT", "(none)", BASE),
            row_full("zlib", "1.3", "r0.0", "cortexa57", "Zlib", "(none)", BASE),
            // shared with the rootfs at the same version…
            row_full("zlib", "1.3", "r0.0", "cortexa57", "Zlib", "(none)", OWN),
            // …but installed in the same transaction as something new
            row_full("curl", "8.7", "r0.1", "cortexa57", "MIT", "(none)", OWN),
        );

        let scopes = cmd(false).parse_scopes(&dump);
        let rt = scopes.iter().find(|s| s.name == "runtime:dev").unwrap();
        let mut names: Vec<&str> = rt.packages.iter().map(|p| p.name.as_str()).collect();
        names.sort();
        assert_eq!(
            names,
            vec!["curl", "zlib"],
            "the overlap is still installed"
        );
    }

    #[test]
    fn a_rootfs_that_later_installs_the_same_package_does_not_erase_it_from_an_extension() {
        // The rootfs installing `jq` at 3000 must not reach back and delete
        // the `jq` this extension installed at 2000 — comparing a row against
        // the rootfs's newest transaction used to do exactly that. The
        // extension's transaction also carries `myapp`, which the rootfs does
        // not have, so the transaction is the extension's and all of it stays.
        let dump = format!(
            "##SCOPE\trootfs\t/rootfs\n{}{}\
             ##SCOPE\text:dev/app\t/runtimes/dev/extensions/app\n{}{}{}",
            row_full("libc6", "2.39", "r0.2", "cortexa57", "MIT", "(none)", 1000),
            row_full("jq", "1.7", "r0.0", "cortexa57", "MIT", "(none)", 3000),
            // seeded: a byte copy of the rootfs row, same id
            row_full("libc6", "2.39", "r0.2", "cortexa57", "MIT", "(none)", 1000),
            // the extension's own transaction
            row_full("jq", "1.7", "r0.0", "cortexa57", "MIT", "(none)", 2000),
            row_full("myapp", "1.0", "r0.0", "cortexa57", "MIT", "(none)", 2000),
        );

        let scopes = cmd(false).parse_scopes(&dump);
        let ext = scopes.iter().find(|s| s.name == "ext:dev/app").unwrap();
        let mut names: Vec<&str> = ext.packages.iter().map(|p| p.name.as_str()).collect();
        names.sort();
        assert_eq!(
            names,
            vec!["jq", "myapp"],
            "the seed goes, the extension's transaction stays whole"
        );
    }

    #[test]
    fn a_row_the_mapping_cannot_read_is_counted_rather_than_silently_dropped() {
        // A tab inside an rpm tag shifts every field after it, so the row
        // cannot be mapped. It is still one package the device holds and the
        // document does not name, which is the thing this command refuses to
        // do quietly for a whole scope.
        let dump = format!(
            "##SCOPE\trootfs\t/rootfs\n{}{}",
            row("libc6", "2.39", "r0.2", "cortexa57", "MIT"),
            "busybox\t1.36.1\ttoo\tfew\tfields\n",
        );

        let scopes = cmd(false).parse_scopes(&dump);
        let rootfs = scopes.iter().find(|s| s.name == "rootfs").unwrap();
        assert_eq!(rootfs.packages.len(), 1);
        assert_eq!(rootfs.unreadable, 1);
    }

    #[test]
    fn vendor_blob_licenses_do_not_reach_the_document_as_bare_identifiers() {
        // `Proprietary` and the `Firmware-*` family are what BSP blobs
        // declare, and neither is on the SPDX license list — so emitted bare
        // they produce an expression no consumer can resolve, on exactly the
        // packages whose licensing someone opened the document to check.
        assert_eq!(spdx_license("Proprietary"), "LicenseRef-Proprietary");
        assert_eq!(spdx_license("Firmware-qcom"), "LicenseRef-Firmware-qcom");
        assert_eq!(
            spdx_license("Firmware-amd-ucode & MIT"),
            "LicenseRef-Firmware-amd-ucode AND MIT"
        );
        // A listed identifier this table has no opinion about still passes
        // through untouched: rewriting one would be a guess.
        assert_eq!(spdx_license("Apache-2.0"), "Apache-2.0");
    }

    #[test]
    fn the_collection_lists_its_own_roots_among_its_elements() {
        // A consumer enumerating `element` must reach the scope packages, or
        // it never reaches the `contains` relationships that say which sysroot
        // holds what.
        let dump = format!(
            "##SCOPE\trootfs\t/rootfs\n{}",
            row("libc6", "2.39", "r0.2", "cortexa57", "MIT")
        );
        let c = cmd(false);
        let doc = c.build_document(&c.parse_scopes(&dump), "qemuarm64", None);
        let sbom = doc["@graph"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["type"] == "software_Sbom")
            .unwrap();

        let elements: Vec<&str> = sbom["element"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e.as_str().unwrap())
            .collect();
        for root in sbom["rootElement"].as_array().unwrap() {
            assert!(
                elements.contains(&root.as_str().unwrap()),
                "root {root} is missing from element"
            );
        }
    }

    #[test]
    fn a_supplier_named_avocado_does_not_land_on_the_tool_agent() {
        // `slug_id("avocado")` is `avocado` unchanged, so a PACKAGER of that
        // bare string collided with the document's own Agent — two nodes, one
        // spdxId, different names.
        let dump = format!(
            "##SCOPE\trootfs\t/rootfs\n{}",
            row_full("libc6", "2.39", "r0.2", "cortexa57", "MIT", "(none)", 1000)
                .replace("Avocado Developers <info@avocadolinux.org>", "avocado")
        );
        let c = cmd(false);
        let doc = c.build_document(&c.parse_scopes(&dump), "qemuarm64", None);

        let mut ids: Vec<&str> = doc["@graph"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == "Agent")
            .map(|e| e["spdxId"].as_str().unwrap())
            .collect();
        let before = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(before, ids.len(), "two Agents share one spdxId: {ids:?}");
    }

    #[test]
    fn the_epoch_reaches_the_purl_and_the_identity() {
        // A purl without the epoch does not match the one a scanner derives
        // from the same rpm, so the CVE match silently misses.
        let mut p = pkg("libc6", "2.39", "r0.2");
        p.epoch = "1".to_string();

        let (_, _, _, _, qualifiers) = parse_purl(&p.purl());
        assert!(qualifiers.contains(&("epoch".to_string(), "1".to_string())));
        assert_eq!(p.evr(), "1:2.39-r0.2");

        // Two packages differing only in epoch are two packages.
        let mut without = p.clone();
        without.epoch = "(none)".to_string();
        assert_ne!(p.key(), without.key());
        assert_eq!(without.evr(), "2.39-r0.2");
        assert!(!without.purl().contains("epoch"));
    }

    #[test]
    fn an_unset_license_is_noassertion_rather_than_a_parenthesised_none() {
        // rpm prints `(none)` for an unset tag, and the parentheses are read as
        // structure: the expression came out as `( none )`, which looks
        // resolvable and is not.
        assert_eq!(spdx_license("(none)"), "NOASSERTION");
        assert_eq!(spdx_license(""), "NOASSERTION");

        // Yocto's marker for a recipe with no redistributable license is not a
        // listed SPDX identifier either.
        assert_eq!(spdx_license("CLOSED"), "LicenseRef-CLOSED");
    }

    #[test]
    fn a_row_with_the_wrong_field_count_is_dropped() {
        // A SUMMARY holding a tab would otherwise put a description where the
        // checksum belongs, and the document would look complete.
        let dump = concat!(
            "##SCOPE\trootfs\t/rootfs\n",
            "truncated\t1.0\tr0.0\n",
            "libc6\t2.39\tr0.2\tcortexa57\t(none)\tMIT\thttps://e.invalid\tp\ts.src.rpm\tabc\t1\t\
             1000\tS\n",
        );
        let scopes = cmd(false).parse_scopes(dump);
        assert_eq!(scopes[0].packages.len(), 1);
        assert_eq!(scopes[0].packages[0].name, "libc6");
    }

    #[test]
    fn the_document_is_byte_stable_across_runs_for_the_same_input() {
        // A consumer diffing two SBOMs must see only what changed on the
        // device, not iteration order.
        let dump = format!(
            "##SCOPE\trootfs\t/rootfs\n{}{}{}",
            row("zlib", "1.3", "r0.0", "cortexa57", "Zlib"),
            row("busybox", "1.36.1", "r0.2", "cortexa57", "GPL-2.0-only"),
            row("libc6", "2.39", "r0.2", "cortexa57", "MIT"),
        );
        let c = cmd(false);
        let scopes = c.parse_scopes(&dump);
        let strip = |v: serde_json::Value| {
            // `created` is the one field that still moves here, because this
            // runs without `SOURCE_DATE_EPOCH`. Set it and nothing does — see
            // `created_timestamp`. Stripped rather than pinned because the env
            // is process-wide and these tests run in parallel.
            serde_json::to_string(&v).unwrap().replace(
                &v["@graph"][0]["created"].as_str().unwrap().to_string(),
                "T",
            )
        };
        assert_eq!(
            strip(c.build_document(&scopes, "qemuarm64", None)),
            strip(c.build_document(&scopes, "qemuarm64", None))
        );
    }

    #[test]
    fn a_rootfs_that_contributed_nothing_is_warned_about_on_its_own() {
        // The seed subtraction needs rootfs rows to compare against. With none
        // — the scope missing from the dump, or every row of it unreadable —
        // nothing is subtracted and the extension keeps its whole copy of the
        // base. The count tripwire cannot see this: it compares against a
        // rootfs count that is now zero.
        //
        // The second extension installed nothing, so it holds no base to be
        // wrong about and naming it would send the reader to a scope with
        // nothing in it.
        let dump = format!(
            "##SCOPE\trootfs\t/rootfs\n\
             ##SCOPE\text:dev/app\t/runtimes/dev/extensions/app\n{}{}\
             ##SCOPE\text:dev/empty\t/runtimes/dev/extensions/empty\n",
            row("libc6", "2.39", "r0.2", "cortexa57", "MIT"),
            row("myapp", "1.0", "r0.0", "cortexa57", "MIT"),
        );

        let scopes = cmd(false).parse_scopes(&dump);
        let warnings = seeding_warnings(&scopes);
        assert_eq!(warnings.len(), 1, "got: {warnings:?}");
        assert!(
            warnings[0].contains("rootfs contributed no packages")
                && warnings[0].contains("ext:dev/app"),
            "the warning has to name the scopes now carrying the base; got: {}",
            warnings[0]
        );
        assert!(
            !warnings[0].contains("ext:dev/empty"),
            "a scope with no packages carries no unsubtracted base; got: {}",
            warnings[0]
        );
    }

    #[test]
    fn a_seeded_scope_as_large_as_the_rootfs_is_warned_about() {
        // The shape a failed subtraction takes: the extension reports the base
        // system as its own content. Same transaction id on every row, so the
        // rows read as one transaction the rootfs does not fully cover.
        //
        // The two counts are made equal rather than the extension made larger,
        // on purpose. A subtraction that failed leaves the extension holding
        // exactly the base, so a tripwire spelled `>` instead of `>=` would
        // miss the case it exists for; only equality pins that.
        let dump = format!(
            "##SCOPE\trootfs\t/rootfs\n{}{}\
             ##SCOPE\text:dev/app\t/runtimes/dev/extensions/app\n{}{}",
            row("libc6", "2.39", "r0.2", "cortexa57", "MIT"),
            row("bash", "5.2", "r0.0", "cortexa57", "GPL-3.0-or-later"),
            row_full("libc6", "2.39", "r0.2", "cortexa57", "MIT", "(none)", 7),
            row_full("myapp", "1.0", "r0.0", "cortexa57", "MIT", "(none)", 7),
        );

        let scopes = cmd(false).parse_scopes(&dump);
        let warnings = seeding_warnings(&scopes);
        assert_eq!(warnings.len(), 1, "got: {warnings:?}");
        assert!(
            warnings[0].contains("at least as many packages as the rootfs")
                && warnings[0].contains("ext:dev/app"),
            "got: {}",
            warnings[0]
        );
    }

    #[test]
    fn a_subtraction_that_worked_produces_no_warning() {
        // The healthy shape, and the one that keeps both guards honest: a
        // rootfs that contributed rows, and an extension left holding only
        // what it installed itself. Neither tripwire may fire here.
        let dump = format!(
            "##SCOPE\trootfs\t/rootfs\n{}{}\
             ##SCOPE\text:dev/app\t/runtimes/dev/extensions/app\n{}{}{}",
            row("libc6", "2.39", "r0.2", "cortexa57", "MIT"),
            row("bash", "5.2", "r0.0", "cortexa57", "GPL-3.0-or-later"),
            row_full("libc6", "2.39", "r0.2", "cortexa57", "MIT", "(none)", 1000),
            row_full(
                "bash",
                "5.2",
                "r0.0",
                "cortexa57",
                "GPL-3.0-or-later",
                "(none)",
                1000
            ),
            row_full("myapp", "1.0", "r0.0", "cortexa57", "MIT", "(none)", 2000),
        );

        let scopes = cmd(false).parse_scopes(&dump);
        assert_eq!(
            scopes
                .iter()
                .find(|s| s.name == "ext:dev/app")
                .unwrap()
                .packages
                .len(),
            1,
            "precondition: the seed was subtracted"
        );
        assert!(seeding_warnings(&scopes).is_empty());
    }

    #[test]
    fn a_single_package_rootfs_is_too_small_to_compare_against() {
        // One package is not evidence either way — every extension carrying a
        // dependency reports at least that many — so the count tripwire stays
        // quiet rather than firing on every scope of a minimal image.
        let dump = format!(
            "##SCOPE\trootfs\t/rootfs\n{}\
             ##SCOPE\text:dev/app\t/runtimes/dev/extensions/app\n{}",
            row("libc6", "2.39", "r0.2", "cortexa57", "MIT"),
            row_full("myapp", "1.0", "r0.0", "cortexa57", "MIT", "(none)", 2000),
        );

        let scopes = cmd(false).parse_scopes(&dump);
        assert!(seeding_warnings(&scopes).is_empty());
    }

    #[test]
    fn ids_are_iri_safe() {
        assert_eq!(slug("ext:dev/app"), "ext-dev-app");
        assert_eq!(slug("2.39+git0+ce65d944e3"), "2.39-git0-ce65d944e3");
        assert_eq!(slug("  --x--  "), "x");
        assert_eq!(
            slug("GPL-2.0-only AND LicenseRef-PD"),
            "GPL-2.0-only-AND-LicenseRef-PD"
        );
    }
}

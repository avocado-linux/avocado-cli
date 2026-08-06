//! Extension fetching utilities for remote extensions.
//!
//! This module provides functionality to fetch extensions from various sources:
//! - Package repository (avocado extension repo)
//! - Git repositories (with optional sparse checkout)
//! - Local filesystem paths (mounted via bindfs at runtime)

use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::utils::config::ExtensionSource;
use crate::utils::container::{RunConfig, SdkContainer};
use crate::utils::output::{print_info, OutputLevel};

/// One package-source extension to materialize in a batched fetch.
///
/// `package_spec` is the fully-resolved NEVRA (or bare name for `version: '*'`)
/// the caller wants installed. It is passed to dnf explicitly so the depsolve
/// cannot substitute a different version than the lock file pinned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageFetchEntry {
    /// Extension name — also the directory it lands in under `includes/`.
    pub ext_name: String,
    /// RPM package name. Usually the extension name, but `source.package` can
    /// rename it. Kept separate from `package_spec` because the layout query
    /// matches on `%{name}` and a spec cannot be split back into name and
    /// version unambiguously.
    pub package_name: String,
    /// Package spec handed to dnf (`<package_name>-<version>`, or just the name).
    pub package_spec: String,
    /// Optional `--repo=` restriction from `source.repo_name`.
    pub repo_name: Option<String>,
}

impl PackageFetchEntry {
    /// Build an entry from a `source: { type: package }` declaration.
    ///
    /// `package` overrides the RPM name when the extension is published under
    /// a different one; otherwise the extension name is the package name.
    /// `version: '*'` means "whatever the feed offers" and produces a bare
    /// name, letting the depsolver choose.
    pub fn from_package_source(
        ext_name: &str,
        package: Option<&str>,
        version: &str,
        repo_name: Option<&str>,
    ) -> Self {
        let package_name = package.unwrap_or(ext_name);
        let package_spec = if version == "*" {
            package_name.to_string()
        } else {
            format!("{package_name}-{version}")
        };
        Self {
            ext_name: ext_name.to_string(),
            package_name: package_name.to_string(),
            package_spec,
            repo_name: repo_name.map(str::to_string),
        }
    }
}

/// Extension fetcher for downloading and installing remote extensions
pub struct ExtensionFetcher {
    /// Path to the main configuration file
    config_path: String,
    /// Target architecture
    target: String,
    /// Enable verbose output
    verbose: bool,
    /// Container image for running fetch operations
    container_image: String,
    /// Repository URL for package fetching
    repo_url: Option<String>,
    /// Repository release for package fetching
    repo_release: Option<String>,
    /// Container arguments
    container_args: Option<Vec<String>>,
    /// SDK container architecture for cross-arch emulation
    sdk_arch: Option<String>,
    /// Source directory for resolving relative extension paths
    src_dir: Option<PathBuf>,
}

impl ExtensionFetcher {
    /// Create a new ExtensionFetcher
    pub fn new(
        config_path: String,
        target: String,
        container_image: String,
        verbose: bool,
    ) -> Self {
        Self {
            config_path,
            target,
            verbose,
            container_image,
            repo_url: None,
            repo_release: None,
            container_args: None,
            sdk_arch: None,
            src_dir: None,
        }
    }

    /// Set repository URL
    pub fn with_repo_url(mut self, repo_url: Option<String>) -> Self {
        self.repo_url = repo_url;
        self
    }

    /// Set repository release
    pub fn with_repo_release(mut self, repo_release: Option<String>) -> Self {
        self.repo_release = repo_release;
        self
    }

    /// Set container arguments
    pub fn with_container_args(mut self, container_args: Option<Vec<String>>) -> Self {
        self.container_args = container_args;
        self
    }

    /// Set SDK container architecture for cross-arch emulation
    pub fn with_sdk_arch(mut self, sdk_arch: Option<String>) -> Self {
        self.sdk_arch = sdk_arch;
        self
    }

    /// Set source directory for resolving relative extension paths
    pub fn with_src_dir(mut self, src_dir: Option<PathBuf>) -> Self {
        self.src_dir = src_dir;
        self
    }

    /// Fetch an extension based on its source configuration
    ///
    /// Returns the path where the extension was installed
    pub async fn fetch(
        &self,
        ext_name: &str,
        source: &ExtensionSource,
        install_dir: &Path,
        force: bool,
    ) -> Result<PathBuf> {
        let ext_install_path = install_dir.join(ext_name);

        match source {
            ExtensionSource::Package {
                version,
                package,
                repo_name,
                ..  // include field not needed for fetching
            } => {
                self.fetch_from_repo(
                    ext_name,
                    version,
                    package.as_deref(),
                    repo_name.as_deref(),
                    &ext_install_path,
                    force,
                )
                .await?;
            }
            ExtensionSource::Git {
                url,
                git_ref,
                sparse_checkout,
                ..  // include field not needed for fetching
            } => {
                self.fetch_from_git(
                    ext_name,
                    url,
                    git_ref.as_deref(),
                    sparse_checkout.as_deref(),
                    &ext_install_path,
                )
                .await?;
            }
            ExtensionSource::Path { path, .. } => {
                self.fetch_from_path(ext_name, path, &ext_install_path)
                    .await?;
            }
        }

        Ok(ext_install_path)
    }

    /// Fetch several package-source extensions in a single DNF transaction.
    ///
    /// This is the path that makes inter-extension dependencies work. Because
    /// nested packages all install into the one shared `includes` installroot,
    /// a single `dnf install a b c` lets the **depsolver** pull in each
    /// extension's `Requires: avocado-ext(<dep>)` closure — dependencies are
    /// discovered, downloaded, and installed in one transaction rather than
    /// through repeated fetch/recompose/discover rounds. It also collapses N
    /// container spin-ups into one.
    ///
    /// Every requested extension is passed explicitly with its locked NEVRA, so
    /// the depsolve cannot drift off the lock file and a consumer's declared
    /// version always beats whatever range a dependent's `Requires:` allows.
    ///
    /// Layout is still detected per package, in-script: only packages providing
    /// `avocado-ext-layout(nested)` may share the installroot. Legacy packages
    /// (content at `/`) are installed one at a time into their own
    /// per-extension installroot exactly as before — they cannot be batched
    /// without colliding.
    /// A `--repo=` restriction applies to the whole transaction, so entries are
    /// grouped by `repo_name` and each group gets its own transaction. In the
    /// overwhelmingly common case every entry has `repo_name: None` and this is
    /// a single group.
    pub async fn fetch_packages(&self, entries: &[PackageFetchEntry], force: bool) -> Result<()> {
        let mut repos: Vec<Option<String>> = Vec::new();
        for e in entries {
            if !repos.contains(&e.repo_name) {
                repos.push(e.repo_name.clone());
            }
        }

        for repo in repos {
            let group: Vec<PackageFetchEntry> = entries
                .iter()
                .filter(|e| e.repo_name == repo)
                .cloned()
                .collect();
            self.fetch_package_group(&group, repo.as_deref(), force)
                .await?;
        }

        Ok(())
    }

    async fn fetch_package_group(
        &self,
        entries: &[PackageFetchEntry],
        repo_name: Option<&str>,
        force: bool,
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        if self.verbose {
            print_info(
                &format!(
                    "Fetching {} package extension(s) in one transaction: {}",
                    entries.len(),
                    entries
                        .iter()
                        .map(|e| e.ext_name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                OutputLevel::Normal,
            );
        }

        // `<ext_name>|<package_name>|<package_spec>` triples. The package name
        // is carried separately rather than parsed back out of the spec:
        // `foo-1.2.0` cannot be split into name and version unambiguously in
        // shell, and the layout query below matches on `%{name}`.
        let triples = entries
            .iter()
            .map(|e| format!("\"{}|{}|{}\"", e.ext_name, e.package_name, e.package_spec))
            .collect::<Vec<_>>()
            .join(" ");
        let repo_arg = repo_name.map(|r| format!("--repo={r}")).unwrap_or_default();
        let force_str = if force { "true" } else { "false" };

        let script = format!(
            r#"
set -e

DNF="RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX"

# Layout detection for the WHOLE set in one query. Asking per package meant one
# dnf metadata load each, which dominated the fetch: five extensions paid five
# full repo loads just to learn where to put them. `--whatprovides` answers it
# for every package at once, still from repo metadata with no payload download.
NESTED_NAMES=$(RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
    $DNF_SDK_HOST $DNF_SDK_HOST_OPTS $DNF_SDK_COMBINED_REPO_CONF {repo_arg} \
    repoquery --whatprovides 'avocado-ext-layout(nested)' --qf '%{{name}}\n' 2>/dev/null | sort -u)

NESTED_SPECS=""
NESTED_NAMES_TO_REMOVE=""
NESTED_DIRS=""

for triple in {triples}; do
    ext_name="${{triple%%|*}}"
    rest="${{triple#*|}}"
    pkg_name="${{rest%%|*}}"
    spec="${{rest#*|}}"
    ext_dir="$AVOCADO_PREFIX/includes/$ext_name"

    if printf '%s\n' "$NESTED_NAMES" | grep -qxF "$pkg_name"; then
        # Accumulate only — every dnf call for the nested set is batched below.
        NESTED_SPECS="$NESTED_SPECS $spec"
        NESTED_NAMES_TO_REMOVE="$NESTED_NAMES_TO_REMOVE $pkg_name"
        NESTED_DIRS="$NESTED_DIRS $ext_dir"
    else
        # Legacy layout: content at /, so it needs its own installroot and
        # cannot join the shared transaction.
        echo "Extension '$ext_name': legacy layout -> per-extension installroot"
        if [ "{force_str}" = "true" ]; then
            rm -rf "$ext_dir"
        fi
        mkdir -p "$ext_dir"
        RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm \
        RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
        $DNF_SDK_HOST $DNF_SDK_HOST_OPTS $DNF_SDK_COMBINED_REPO_CONF {repo_arg} \
            --installroot="$ext_dir" -y install "$spec"
    fi
done

# --force: clear the whole nested set in ONE transaction. Removing per
# extension meant a full dnf metadata load each, which is what dominated a
# forced re-fetch — the install itself was already batched.
if [ "{force_str}" = "true" ] && [ -n "$NESTED_NAMES_TO_REMOVE" ]; then
    echo "Removing nested extensions for re-fetch:$NESTED_NAMES_TO_REMOVE"
    RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
        $DNF_SDK_HOST $DNF_SDK_HOST_OPTS --installroot="$AVOCADO_PREFIX/includes" \
        -y remove $NESTED_NAMES_TO_REMOVE 2>/dev/null || true
    for d in $NESTED_DIRS; do rm -rf "$d"; done
fi

if [ -n "$NESTED_SPECS" ]; then
    echo "Installing nested extensions into shared includes installroot:$NESTED_SPECS"
    mkdir -p "$AVOCADO_PREFIX/includes"
    # One transaction: dnf resolves each package's `Requires: avocado-ext(...)`
    # and pulls in any dependency extensions not named explicitly here.
    RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm \
    RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
    $DNF_SDK_HOST $DNF_SDK_HOST_OPTS $DNF_SDK_COMBINED_REPO_CONF {repo_arg} \
        --installroot="$AVOCADO_PREFIX/includes" -y install $NESTED_SPECS
fi
"#
        );

        let container_helper = SdkContainer::new().verbose(self.verbose);
        let run_config = RunConfig {
            container_image: self.container_image.clone(),
            target: self.target.clone(),
            command: script,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: self.repo_url.clone(),
            repo_release: self.repo_release.clone(),
            container_args: self.container_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };

        if !container_helper.run_in_container(run_config).await? {
            return Err(anyhow::anyhow!(
                "Failed to fetch package extension(s): {}",
                entries
                    .iter()
                    .map(|e| e.ext_name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        Ok(())
    }

    /// Fetch an extension from the avocado package repository
    ///
    /// Installs the extension package into the SHARED `$AVOCADO_PREFIX/includes`
    /// installroot using DNF with `--installroot`. Packages nest their content under a
    /// top-level `/<ext_name>/` dir, so the content lands at `includes/<ext_name>/` and a
    /// single rpmdb tracks every installed extension (proper tracking, clean upgrades,
    /// version management, no cross-extension file collisions).
    #[allow(dead_code)]
    async fn fetch_from_repo(
        &self,
        ext_name: &str,
        version: &str,
        package: Option<&str>,
        repo_name: Option<&str>,
        _install_path: &Path, // Host path - not used, we use container path instead
        force: bool,
    ) -> Result<()> {
        // Use explicit package name if provided, otherwise fall back to extension name
        let package_name = package.unwrap_or(ext_name);

        if self.verbose {
            print_info(
                &format!(
                    "Fetching extension '{ext_name}' (package: '{package_name}') version '{version}' from package repository"
                ),
                OutputLevel::Normal,
            );
        }

        // Build the package spec using the package name (not extension name)
        let package_spec = if version == "*" {
            package_name.to_string()
        } else {
            format!("{package_name}-{version}")
        };

        let repo_arg = repo_name.map(|r| format!("--repo={r}")).unwrap_or_default();

        // The package self-describes its layout via `Provides: avocado-ext-layout(nested)`.
        // - NESTED (new): content under /<ext_name>/ -> install into the SHARED includes
        //   installroot, so it lands at includes/<ext_name>/ with one rpmdb tracking all exts.
        // - LEGACY (no such provide): content at / -> per-extension installroot includes/<ext_name>.
        // Either way the final content is includes/<ext_name>/, so consumers are unchanged. The
        // installroot is chosen at run time by repoquerying the package's provides.
        let ext_dir = format!("$AVOCADO_PREFIX/includes/{ext_name}");
        let force_str = if force { "true" } else { "false" };

        // Install the extension package using DNF with --installroot
        // Uses $DNF_SDK_COMBINED_REPO_CONF to access both SDK and target-specific repos
        let fetch_script = format!(
            r#"
set -e

# Detect the package's on-disk layout from its provides (repo metadata, no download).
if RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
   $DNF_SDK_HOST $DNF_SDK_HOST_OPTS $DNF_SDK_COMBINED_REPO_CONF {repo_arg} \
   repoquery --provides {package_spec} 2>/dev/null | grep -q 'avocado-ext-layout(nested)'; then
    INSTALLROOT="$AVOCADO_PREFIX/includes"
    echo "Extension '{ext_name}': nested layout -> shared includes installroot"
else
    INSTALLROOT="{ext_dir}"
    echo "Extension '{ext_name}': legacy layout -> per-extension installroot"
fi

# Force: remove just this extension (rpmdb entry + content dir) for a clean reinstall,
# without disturbing other extensions sharing the installroot.
if [ "{force_str}" = "true" ]; then
    RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
        $DNF_SDK_HOST $DNF_SDK_HOST_OPTS --installroot="$INSTALLROOT" -y remove {package_name} 2>/dev/null || true
    rm -rf "{ext_dir}"
fi

mkdir -p "$INSTALLROOT"

RPM_CONFIGDIR=$AVOCADO_SDK_PREFIX/usr/lib/rpm \
RPM_ETCCONFIGDIR=$AVOCADO_SDK_PREFIX \
$DNF_SDK_HOST \
    $DNF_SDK_HOST_OPTS \
    $DNF_SDK_COMBINED_REPO_CONF \
    {repo_arg} \
    --installroot="$INSTALLROOT" \
    -y \
    install \
    {package_spec}

echo "Successfully installed extension '{ext_name}' (package: {package_spec}) to {ext_dir}"
"#
        );

        let container_helper = SdkContainer::new().verbose(self.verbose);
        let run_config = RunConfig {
            container_image: self.container_image.clone(),
            target: self.target.clone(),
            command: fetch_script,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: self.repo_url.clone(),
            repo_release: self.repo_release.clone(),
            container_args: self.container_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };

        let success = container_helper.run_in_container(run_config).await?;
        if !success {
            return Err(anyhow::anyhow!(
                "Failed to fetch extension '{ext_name}' from package repository"
            ));
        }

        Ok(())
    }

    /// Fetch an extension from a git repository
    async fn fetch_from_git(
        &self,
        ext_name: &str,
        url: &str,
        git_ref: Option<&str>,
        sparse_checkout: Option<&[String]>,
        _install_path: &Path, // Host path - not used, we use container path instead
    ) -> Result<()> {
        if self.verbose {
            print_info(
                &format!("Fetching extension '{ext_name}' from git: {url}"),
                OutputLevel::Normal,
            );
        }

        // Use container path $AVOCADO_PREFIX/includes/<ext_name> instead of host path
        let container_install_path = format!("$AVOCADO_PREFIX/includes/{ext_name}");
        let ref_arg = git_ref.unwrap_or("HEAD");

        // Build the git clone command
        let git_cmd = if let Some(sparse_paths) = sparse_checkout {
            // Use sparse checkout for specific paths
            let sparse_paths_str = sparse_paths.join(" ");
            format!(
                r#"
set -e
rm -rf "{container_install_path}"
mkdir -p "{container_install_path}"
cd "{container_install_path}"
git init
git remote add origin "{url}"
git config core.sparseCheckout true
echo "{sparse_paths_str}" | tr ' ' '\n' > .git/info/sparse-checkout
git fetch --depth 1 origin {ref_arg}
git checkout FETCH_HEAD
# Move sparse checkout contents to root if needed
if [ -d "{sparse_paths_str}" ]; then
    mv {sparse_paths_str}/* . 2>/dev/null || true
    rm -rf {sparse_paths_str}
fi
echo "Successfully fetched extension '{ext_name}' from git"
"#
            )
        } else {
            // Full clone
            format!(
                r#"
set -e
rm -rf "{container_install_path}"
git clone --depth 1 --branch {ref_arg} "{url}" "{container_install_path}" || \
git clone --depth 1 "{url}" "{container_install_path}"
cd "{container_install_path}"
if [ "{ref_arg}" != "HEAD" ]; then
    git checkout {ref_arg} 2>/dev/null || true
fi
echo "Successfully fetched extension '{ext_name}' from git"
"#
            )
        };

        let container_helper = SdkContainer::new().verbose(self.verbose);
        let run_config = RunConfig {
            container_image: self.container_image.clone(),
            target: self.target.clone(),
            command: git_cmd,
            verbose: self.verbose,
            source_environment: true,
            interactive: false,
            repo_url: self.repo_url.clone(),
            repo_release: self.repo_release.clone(),
            container_args: self.container_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };

        let success = container_helper.run_in_container(run_config).await?;
        if !success {
            return Err(anyhow::anyhow!(
                "Failed to fetch extension '{ext_name}' from git repository"
            ));
        }

        Ok(())
    }

    /// Fetch an extension from a local filesystem path
    ///
    /// Instead of copying files, this validates the path exists and stores the
    /// mapping for bindfs mounting at container runtime. The extension source
    /// will be mounted at `/mnt/ext/<ext_name>` and bindfs'd to
    /// `$AVOCADO_PREFIX/includes/<ext_name>`.
    async fn fetch_from_path(
        &self,
        ext_name: &str,
        source_path: &str,
        _install_path: &Path, // Host path - not used, we use bindfs mounting instead
    ) -> Result<()> {
        if self.verbose {
            print_info(
                &format!("Registering extension '{ext_name}' from path: {source_path}"),
                OutputLevel::Normal,
            );
        }

        // Resolve the source path relative to src_dir (or config dir if src_dir not set)
        let resolved_source = if Path::new(source_path).is_absolute() {
            PathBuf::from(source_path)
        } else {
            // Use src_dir if available, otherwise fall back to config directory
            if let Some(ref src_dir) = self.src_dir {
                src_dir.join(source_path)
            } else {
                let config_dir = Path::new(&self.config_path)
                    .parent()
                    .unwrap_or(Path::new("."));
                config_dir.join(source_path)
            }
        };

        // Canonicalize the path to get the absolute path
        let resolved_source = resolved_source.canonicalize().unwrap_or(resolved_source);

        if !resolved_source.exists() {
            return Err(anyhow::anyhow!(
                "Extension source path does not exist: {}\n\
                 Path was resolved relative to: {}",
                resolved_source.display(),
                self.src_dir
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "config directory".to_string())
            ));
        }

        // Check that the path contains an avocado.yaml or avocado.yml file
        let has_config = resolved_source.join("avocado.yaml").exists()
            || resolved_source.join("avocado.yml").exists();
        if !has_config {
            return Err(anyhow::anyhow!(
                "Extension source path does not contain an avocado.yaml or avocado.yml file: {}",
                resolved_source.display()
            ));
        }

        // No state file is written: `type: path` mounts are derived directly
        // from avocado.yaml at container launch (see
        // `SdkContainer::derive_ext_path_mounts`), so the config stays the
        // single source of truth and can't drift. `ext fetch` for a path
        // extension is now purely validation (the checks above).
        print_info(
            &format!(
                "Extension '{ext_name}' will be mounted via bindfs at runtime from: {}",
                resolved_source.display()
            ),
            OutputLevel::Normal,
        );

        Ok(())
    }

    /// Check if an extension is already fetched/installed
    pub fn is_extension_installed(install_dir: &Path, ext_name: &str) -> bool {
        let ext_path = install_dir.join(ext_name);
        // Check if the directory exists and has an avocado config file
        ext_path.exists()
            && (ext_path.join("avocado.yaml").exists() || ext_path.join("avocado.yml").exists())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_package_entry_uses_extension_name_by_default() {
        let e = PackageFetchEntry::from_package_source("weston-base", None, "1.2.0", None);
        assert_eq!(e.ext_name, "weston-base");
        assert_eq!(e.package_spec, "weston-base-1.2.0");
        assert_eq!(e.repo_name, None);
    }

    #[test]
    fn test_package_entry_keeps_name_separate_from_spec() {
        // The layout query matches on %{name}; `app-a-0.1.0` cannot be split
        // back into name and version in shell, so the name is carried.
        let e = PackageFetchEntry::from_package_source("app-a", None, "0.1.0", None);
        assert_eq!(e.package_name, "app-a");
        assert_eq!(e.package_spec, "app-a-0.1.0");
    }

    #[test]
    fn test_package_entry_honors_a_package_rename() {
        // `source.package` publishes the extension under a different RPM name.
        // The install spec must use it, while the extension name (and so the
        // includes/<name>/ directory) stays as declared.
        let e = PackageFetchEntry::from_package_source(
            "weston-base",
            Some("avocado-ext-weston-base"),
            "1.2.0",
            None,
        );
        assert_eq!(e.ext_name, "weston-base");
        assert_eq!(e.package_name, "avocado-ext-weston-base");
        assert_eq!(e.package_spec, "avocado-ext-weston-base-1.2.0");
    }

    #[test]
    fn test_package_entry_wildcard_version_is_an_unpinned_spec() {
        // `*` hands version choice to the depsolver rather than pinning.
        let e = PackageFetchEntry::from_package_source("app-a", None, "*", None);
        assert_eq!(e.package_spec, "app-a");
    }

    #[test]
    fn test_package_entry_carries_repo_restriction() {
        let e = PackageFetchEntry::from_package_source("app-a", None, "1.0.0", Some("my-repo"));
        assert_eq!(e.repo_name.as_deref(), Some("my-repo"));
    }

    #[test]
    fn test_extension_fetcher_creation() {
        let fetcher = ExtensionFetcher::new(
            "avocado.yaml".to_string(),
            "x86_64-unknown-linux-gnu".to_string(),
            "docker.io/avocadolinux/sdk:latest".to_string(),
            false,
        );

        assert!(!fetcher.verbose);
        assert_eq!(fetcher.target, "x86_64-unknown-linux-gnu");
    }

    #[test]
    fn test_is_extension_installed() {
        // This would need a temp directory to test properly
        // For now just verify the function exists
        let result =
            ExtensionFetcher::is_extension_installed(Path::new("/nonexistent"), "test-ext");
        assert!(!result);
    }

    #[test]
    fn test_is_extension_installed_with_config() {
        let tmp_dir = std::env::temp_dir().join("avocado_test_ext_installed");
        let ext_dir = tmp_dir.join("my-ext");
        let _ = std::fs::remove_dir_all(&tmp_dir);
        std::fs::create_dir_all(&ext_dir).unwrap();

        // Directory exists but no config file -> not installed
        assert!(!ExtensionFetcher::is_extension_installed(
            &tmp_dir, "my-ext"
        ));

        // Add avocado.yaml -> installed
        std::fs::write(ext_dir.join("avocado.yaml"), "version: '1.0.0'").unwrap();
        assert!(ExtensionFetcher::is_extension_installed(&tmp_dir, "my-ext"));

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}

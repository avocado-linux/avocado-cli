//! SBOM (Software Bill of Materials) generation utilities.
//!
//! This module provides functionality to generate SPDX-formatted SBOMs
//! from RPM package databases in sysroots.

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Represents a package in the SBOM
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct SbomPackage {
    /// Package name
    pub name: String,
    /// Package version (VERSION-RELEASE)
    pub version: String,
    /// Package architecture
    pub arch: String,
    /// Package license (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Package vendor (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// Package summary/description (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Source of the package (e.g., "runtime", "extension:my-ext")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl SbomPackage {
    /// Create a unique identifier for deduplication
    pub fn unique_id(&self) -> String {
        format!("{}-{}.{}", self.name, self.version, self.arch)
    }

    /// Create an SPDX package ID (must be unique within the document)
    pub fn spdx_id(&self) -> String {
        // SPDX IDs must only contain letters, numbers, dots, and hyphens
        let sanitized_name = self
            .name
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '.' { c } else { '-' })
            .collect::<String>();
        let sanitized_version = self
            .version
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '.' { c } else { '-' })
            .collect::<String>();
        format!("SPDXRef-Package-{}-{}", sanitized_name, sanitized_version)
    }
}

/// SPDX document representing an SBOM
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpdxDocument {
    /// SPDX version
    pub spdx_version: String,
    /// Data license for the SPDX document itself
    pub data_license: String,
    /// SPDX document identifier
    #[serde(rename = "SPDXID")]
    pub spdx_id: String,
    /// Document name
    pub name: String,
    /// Document namespace (unique URI)
    pub document_namespace: String,
    /// Creation info
    pub creation_info: SpdxCreationInfo,
    /// Packages in the document
    pub packages: Vec<SpdxPackage>,
    /// Relationships between packages
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub relationships: Vec<SpdxRelationship>,
}

/// SPDX creation information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpdxCreationInfo {
    /// Creation timestamp in ISO 8601 format
    pub created: String,
    /// Tools used to create the SBOM
    pub creators: Vec<String>,
}

/// SPDX package representation
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpdxPackage {
    /// Unique identifier within the document
    #[serde(rename = "SPDXID")]
    pub spdx_id: String,
    /// Package name
    pub name: String,
    /// Package version
    pub version_info: String,
    /// Download location (NOASSERTION for RPM packages without source)
    pub download_location: String,
    /// Files analyzed flag
    pub files_analyzed: bool,
    /// License concluded
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license_concluded: Option<String>,
    /// License declared
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license_declared: Option<String>,
    /// Copyright text
    pub copyright_text: String,
    /// Package supplier
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supplier: Option<String>,
    /// Package summary
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// External references (e.g., purl)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub external_refs: Vec<SpdxExternalRef>,
    /// Package annotations (e.g., source sysroot)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub annotations: Vec<SpdxAnnotation>,
}

/// SPDX external reference
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpdxExternalRef {
    /// Reference category
    pub reference_category: String,
    /// Reference type
    pub reference_type: String,
    /// Reference locator
    pub reference_locator: String,
}

/// SPDX annotation
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpdxAnnotation {
    /// Annotation date
    pub annotation_date: String,
    /// Annotation type
    pub annotation_type: String,
    /// Annotator
    pub annotator: String,
    /// Comment
    pub comment: String,
}

/// SPDX relationship
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpdxRelationship {
    /// Source element ID
    pub spdx_element_id: String,
    /// Relationship type
    pub relationship_type: String,
    /// Related element ID
    pub related_spdx_element: String,
}

/// Builder for creating SPDX documents
pub struct SpdxBuilder {
    name: String,
    namespace_base: String,
    packages: Vec<SbomPackage>,
}

impl SpdxBuilder {
    /// Create a new SPDX builder
    pub fn new(name: &str, namespace_base: &str) -> Self {
        Self {
            name: name.to_string(),
            namespace_base: namespace_base.to_string(),
            packages: Vec::new(),
        }
    }

    /// Add packages to the SBOM
    pub fn add_packages(&mut self, packages: Vec<SbomPackage>) -> &mut Self {
        self.packages.extend(packages);
        self
    }

    /// Deduplicate packages by unique ID, keeping track of all sources
    pub fn deduplicate(&mut self) -> &mut Self {
        let mut seen: HashMap<String, SbomPackage> = HashMap::new();
        let mut sources: HashMap<String, Vec<String>> = HashMap::new();

        for pkg in &self.packages {
            let id = pkg.unique_id();
            if let Some(source) = &pkg.source {
                sources
                    .entry(id.clone())
                    .or_default()
                    .push(source.clone());
            }
            seen.entry(id).or_insert_with(|| pkg.clone());
        }

        // Update sources to show all origins
        self.packages = seen
            .into_iter()
            .map(|(id, mut pkg)| {
                if let Some(pkg_sources) = sources.get(&id) {
                    let unique_sources: HashSet<_> = pkg_sources.iter().collect();
                    if unique_sources.len() > 1 {
                        pkg.source = Some(
                            unique_sources
                                .into_iter()
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(", "),
                        );
                    }
                }
                pkg
            })
            .collect();

        self
    }

    /// Build the SPDX document
    pub fn build(&self) -> SpdxDocument {
        let timestamp = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let doc_uuid = uuid::Uuid::new_v4();

        let spdx_packages: Vec<SpdxPackage> = self
            .packages
            .iter()
            .map(|pkg| {
                let purl = format!(
                    "pkg:rpm/avocado/{}@{}?arch={}",
                    pkg.name, pkg.version, pkg.arch
                );

                let external_refs = vec![SpdxExternalRef {
                    reference_category: "PACKAGE-MANAGER".to_string(),
                    reference_type: "purl".to_string(),
                    reference_locator: purl,
                }];

                // Add annotations for source if present
                let annotations = if let Some(source) = &pkg.source {
                    vec![SpdxAnnotation {
                        annotation_date: timestamp.clone(),
                        annotation_type: "OTHER".to_string(),
                        annotator: "Tool: avocado-cli".to_string(),
                        comment: format!("Package source: {}", source),
                    }]
                } else {
                    vec![]
                };

                SpdxPackage {
                    spdx_id: pkg.spdx_id(),
                    name: pkg.name.clone(),
                    version_info: format!("{}.{}", pkg.version, pkg.arch),
                    download_location: "NOASSERTION".to_string(),
                    files_analyzed: false,
                    license_concluded: pkg.license.clone().or(Some("NOASSERTION".to_string())),
                    license_declared: pkg.license.clone().or(Some("NOASSERTION".to_string())),
                    copyright_text: "NOASSERTION".to_string(),
                    supplier: pkg.vendor.as_ref().map(|v| format!("Organization: {}", v)),
                    summary: pkg.summary.clone(),
                    external_refs,
                    annotations,
                }
            })
            .collect();

        // Create relationships: all packages are contained by the document
        let relationships: Vec<SpdxRelationship> = spdx_packages
            .iter()
            .map(|pkg| SpdxRelationship {
                spdx_element_id: "SPDXRef-DOCUMENT".to_string(),
                relationship_type: "DESCRIBES".to_string(),
                related_spdx_element: pkg.spdx_id.clone(),
            })
            .collect();

        SpdxDocument {
            spdx_version: "SPDX-2.3".to_string(),
            data_license: "CC0-1.0".to_string(),
            spdx_id: "SPDXRef-DOCUMENT".to_string(),
            name: self.name.clone(),
            document_namespace: format!("{}/{}", self.namespace_base, doc_uuid),
            creation_info: SpdxCreationInfo {
                created: timestamp,
                creators: vec![
                    "Tool: avocado-cli".to_string(),
                    "Organization: Avocado Linux".to_string(),
                ],
            },
            packages: spdx_packages,
            relationships,
        }
    }

    /// Build and serialize to JSON
    pub fn to_json(&self) -> Result<String> {
        let doc = self.build();
        Ok(serde_json::to_string_pretty(&doc)?)
    }
}

/// Build an RPM query command to get package info for SBOM generation
/// Returns the shell command to execute in the container
pub fn build_rpm_query_all_command(root_path: Option<&str>) -> String {
    // Query format: NAME|VERSION-RELEASE|ARCH|LICENSE|VENDOR|SUMMARY
    let query_format = "%{NAME}|%{VERSION}-%{RELEASE}|%{ARCH}|%{LICENSE}|%{VENDOR}|%{SUMMARY}\\n";

    if let Some(root) = root_path {
        format!(
            "(unset RPM_ETCCONFIGDIR RPM_CONFIGDIR; rpm -qa --root=\"{}\" --qf '{}') || true",
            root, query_format
        )
    } else {
        format!("rpm -qa --qf '{}' || true", query_format)
    }
}

/// Parse RPM query output into SbomPackage structs
pub fn parse_rpm_query_output(output: &str, source: Option<&str>) -> Vec<SbomPackage> {
    let mut packages = Vec::new();

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Skip info/error lines from container output
        if line.starts_with("[INFO]")
            || line.starts_with("[ERROR]")
            || line.starts_with("[SUCCESS]")
            || line.starts_with("[DEBUG]")
            || line.starts_with("[WARNING]")
        {
            continue;
        }

        // Split on pipe delimiter: NAME|VERSION-RELEASE|ARCH|LICENSE|VENDOR|SUMMARY
        let parts: Vec<&str> = line.splitn(6, '|').collect();
        if parts.len() >= 3 {
            let name = parts[0].to_string();
            let version = parts[1].to_string();
            let arch = parts[2].to_string();

            // Skip if name looks invalid
            if name.is_empty() || name.starts_with('[') {
                continue;
            }

            let license = parts.get(3).and_then(|s| {
                let s = s.trim();
                if s.is_empty() || s == "(none)" {
                    None
                } else {
                    Some(s.to_string())
                }
            });

            let vendor = parts.get(4).and_then(|s| {
                let s = s.trim();
                if s.is_empty() || s == "(none)" {
                    None
                } else {
                    Some(s.to_string())
                }
            });

            let summary = parts.get(5).and_then(|s| {
                let s = s.trim();
                if s.is_empty() || s == "(none)" {
                    None
                } else {
                    Some(s.to_string())
                }
            });

            packages.push(SbomPackage {
                name,
                version,
                arch,
                license,
                vendor,
                summary,
                source: source.map(|s| s.to_string()),
            });
        }
    }

    packages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rpm_query_output() {
        let output = r#"curl|7.88.1-r0|x86_64|MIT|Avocado|Command line tool for transferring data
openssl|3.0.8-r0|x86_64|Apache-2.0|Avocado|Cryptography and SSL/TLS toolkit
bash|5.2-r0|x86_64|(none)|(none)|The GNU Bourne Again shell
"#;

        let packages = parse_rpm_query_output(output, Some("runtime"));

        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "curl");
        assert_eq!(packages[0].version, "7.88.1-r0");
        assert_eq!(packages[0].arch, "x86_64");
        assert_eq!(packages[0].license, Some("MIT".to_string()));
        assert_eq!(packages[0].vendor, Some("Avocado".to_string()));
        assert_eq!(packages[0].source, Some("runtime".to_string()));

        assert_eq!(packages[2].name, "bash");
        assert_eq!(packages[2].license, None);
        assert_eq!(packages[2].vendor, None);
    }

    #[test]
    fn test_parse_rpm_query_output_filters_info_lines() {
        let output = r#"[INFO] Using repo URL: 'http://192.168.1.10:8080'
curl|7.88.1-r0|x86_64|MIT|Avocado|A tool
[ERROR] Some error
wget|1.21-r0|x86_64|GPL-3.0|GNU|A downloader
"#;

        let packages = parse_rpm_query_output(output, None);

        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "curl");
        assert_eq!(packages[1].name, "wget");
    }

    #[test]
    fn test_spdx_builder_basic() {
        let packages = vec![
            SbomPackage {
                name: "curl".to_string(),
                version: "7.88.1-r0".to_string(),
                arch: "x86_64".to_string(),
                license: Some("MIT".to_string()),
                vendor: Some("Avocado".to_string()),
                summary: Some("A transfer tool".to_string()),
                source: Some("runtime".to_string()),
            },
        ];

        let mut builder = SpdxBuilder::new("test-sbom", "https://avocado.dev/sbom");
        builder.add_packages(packages);

        let doc = builder.build();

        assert_eq!(doc.spdx_version, "SPDX-2.3");
        assert_eq!(doc.name, "test-sbom");
        assert_eq!(doc.packages.len(), 1);
        assert_eq!(doc.packages[0].name, "curl");
    }

    #[test]
    fn test_spdx_builder_deduplication() {
        let packages = vec![
            SbomPackage {
                name: "curl".to_string(),
                version: "7.88.1-r0".to_string(),
                arch: "x86_64".to_string(),
                license: None,
                vendor: None,
                summary: None,
                source: Some("runtime".to_string()),
            },
            SbomPackage {
                name: "curl".to_string(),
                version: "7.88.1-r0".to_string(),
                arch: "x86_64".to_string(),
                license: None,
                vendor: None,
                summary: None,
                source: Some("extension:my-ext".to_string()),
            },
        ];

        let mut builder = SpdxBuilder::new("test-sbom", "https://avocado.dev/sbom");
        builder.add_packages(packages);
        builder.deduplicate();

        let doc = builder.build();

        assert_eq!(doc.packages.len(), 1);
        assert_eq!(doc.packages[0].name, "curl");
    }

    #[test]
    fn test_build_rpm_query_all_command() {
        let cmd = build_rpm_query_all_command(Some("$AVOCADO_PREFIX/runtimes/dev"));
        assert!(cmd.contains("rpm -qa"));
        assert!(cmd.contains("--root"));
        assert!(cmd.contains("$AVOCADO_PREFIX/runtimes/dev"));
        assert!(cmd.contains("unset RPM_ETCCONFIGDIR"));

        let cmd_no_root = build_rpm_query_all_command(None);
        assert!(cmd_no_root.contains("rpm -qa"));
        assert!(!cmd_no_root.contains("--root"));
    }

    #[test]
    fn test_sbom_package_spdx_id() {
        let pkg = SbomPackage {
            name: "my-package".to_string(),
            version: "1.0.0-r0".to_string(),
            arch: "x86_64".to_string(),
            license: None,
            vendor: None,
            summary: None,
            source: None,
        };

        let id = pkg.spdx_id();
        assert!(id.starts_with("SPDXRef-Package-"));
        assert!(id.contains("my-package"));
    }
}


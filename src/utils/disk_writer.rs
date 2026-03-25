use anyhow::{Context, Result};
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::Command;

/// A removable disk detected on the host, validated as safe to write.
#[derive(Debug)]
pub struct RemovableDisk {
    /// macOS disk identifier (e.g., "disk2")
    pub identifier: String,
    /// Human-readable device name
    pub name: String,
    /// Size in bytes
    pub size_bytes: u64,
    /// Bus protocol (e.g., "USB", "Secure Digital")
    pub bus_type: String,
}

impl RemovableDisk {
    /// Format size for display (e.g., "15.9 GB")
    fn display_size(&self) -> String {
        let gb = self.size_bytes as f64 / 1_000_000_000.0;
        if gb >= 1.0 {
            format!("{:.1} GB", gb)
        } else {
            let mb = self.size_bytes as f64 / 1_000_000.0;
            format!("{:.0} MB", mb)
        }
    }
}

pub struct DiskWriter {
    verbose: bool,
}

impl DiskWriter {
    pub fn new(verbose: bool) -> Self {
        Self { verbose }
    }

    /// Detect removable storage, prompt user, and burn the image.
    pub fn burn_to_removable(&self, image_path: &Path, _force: bool) -> Result<()> {
        if !image_path.exists() {
            return Err(anyhow::anyhow!(
                "Disk image not found: {}",
                image_path.display()
            ));
        }

        let image_size = std::fs::metadata(image_path)?.len();

        println!();
        println!("==========================================");
        println!("Avocado Host-Side SD Card Writer");
        println!("==========================================");
        println!();

        // Detect removable disks with retry
        let disks = self.detect_with_retry()?;

        // Select disk
        let disk = if disks.len() == 1 {
            println!("Found removable device:");
            self.print_disk_info(&disks[0]);
            &disks[0]
        } else {
            println!("Found {} removable devices:", disks.len());
            for (i, d) in disks.iter().enumerate() {
                println!("  [{}] /dev/{} - {} ({}, {})", i + 1, d.identifier, d.name, d.display_size(), d.bus_type);
            }
            println!();
            let idx = self.prompt_disk_selection(disks.len())?;
            &disks[idx]
        };

        // Mandatory destruction warning - never skipped
        println!();
        println!("==========================================");
        println!("  WARNING: ALL DATA ON THIS DEVICE");
        println!("  WILL BE PERMANENTLY DESTROYED");
        println!("==========================================");
        println!();
        self.print_disk_info(disk);
        println!("  Image: {} ({})", image_path.file_name().unwrap_or_default().to_string_lossy(), format_bytes(image_size));
        println!();

        if !self.confirm("Are you sure you want to write to this device? (y/N): ")? {
            println!("Operation cancelled.");
            return Ok(());
        }

        // Unmount
        println!();
        println!("Unmounting /dev/{}...", disk.identifier);
        let status = Command::new("diskutil")
            .args(["unmountDisk", &format!("/dev/{}", disk.identifier)])
            .status()
            .context("Failed to run diskutil unmountDisk")?;
        if !status.success() {
            return Err(anyhow::anyhow!("Failed to unmount /dev/{}", disk.identifier));
        }

        // Write with dd via sudo, using raw device for speed
        let raw_device = format!("/dev/r{}", disk.identifier);
        println!("Writing image to {}...", raw_device);
        println!("(sudo may prompt for your password)");

        let status = Command::new("sudo")
            .args([
                "dd",
                &format!("if={}", image_path.display()),
                &format!("of={}", raw_device),
                "bs=1m",
            ])
            .status()
            .context("Failed to run dd")?;

        if !status.success() {
            return Err(anyhow::anyhow!("dd failed to write image to {}", raw_device));
        }

        // Eject
        println!("Ejecting /dev/{}...", disk.identifier);
        let _ = Command::new("diskutil")
            .args(["eject", &format!("/dev/{}", disk.identifier)])
            .status();

        println!();
        println!("SD card written successfully!");
        println!("You can now safely remove the SD card and use it to boot your device.");

        Ok(())
    }

    /// Detect removable disks, prompting user to insert one if none found.
    fn detect_with_retry(&self) -> Result<Vec<RemovableDisk>> {
        let disks = self.detect_removable_disks()?;
        if !disks.is_empty() {
            return Ok(disks);
        }

        println!("No removable storage devices detected.");
        println!("Please insert an SD card and press Enter to continue...");
        let _ = std::io::stdin().lock().read_line(&mut String::new());

        // Retry with timeout
        let timeout = std::time::Duration::from_secs(60);
        let start = std::time::Instant::now();
        let poll_interval = std::time::Duration::from_secs(2);

        while start.elapsed() < timeout {
            std::thread::sleep(poll_interval);
            print!(".");
            std::io::stdout().flush()?;

            let disks = self.detect_removable_disks()?;
            if !disks.is_empty() {
                println!();
                return Ok(disks);
            }
        }

        println!();
        Err(anyhow::anyhow!(
            "Timed out after 60 seconds waiting for removable storage device"
        ))
    }

    /// List ONLY removable disks safe for writing. Never fixed/system drives.
    ///
    /// Enumerates both external USB devices and built-in SD card readers.
    /// macOS built-in SD readers are classified as "internal" but the media
    /// is removable — these are included when bus type is "Secure Digital".
    fn detect_removable_disks(&self) -> Result<Vec<RemovableDisk>> {
        let mut all_disk_ids = Vec::new();

        // First: external bus devices (USB card readers, USB drives)
        if let Ok(output) = Command::new("diskutil")
            .args(["list", "-plist", "external"])
            .output()
        {
            if output.status.success() {
                let plist_str = String::from_utf8_lossy(&output.stdout);
                all_disk_ids.extend(parse_disk_identifiers_from_plist(&plist_str));
            }
        }

        // Second: internal devices — needed for built-in SD card readers.
        // We enumerate all disks and rely on validate_disk() to filter safely.
        if let Ok(output) = Command::new("diskutil")
            .args(["list", "-plist"])
            .output()
        {
            if output.status.success() {
                let plist_str = String::from_utf8_lossy(&output.stdout);
                for id in parse_disk_identifiers_from_plist(&plist_str) {
                    if !all_disk_ids.contains(&id) {
                        all_disk_ids.push(id);
                    }
                }
            }
        }

        if self.verbose {
            eprintln!("[DEBUG] Candidate disks found: {:?}", all_disk_ids);
        }

        // Validate each disk — only truly removable media on safe buses pass
        let mut disks = Vec::new();
        for disk_id in &all_disk_ids {
            if let Some(disk) = self.validate_disk(disk_id)? {
                disks.push(disk);
            }
        }

        Ok(disks)
    }

    /// Validate a single disk is safe to write (removable, external, USB/SD bus).
    fn validate_disk(&self, disk_id: &str) -> Result<Option<RemovableDisk>> {
        let output = Command::new("diskutil")
            .args(["info", "-plist", &format!("/dev/{}", disk_id)])
            .output()
            .with_context(|| format!("Failed to run diskutil info for {}", disk_id))?;

        if !output.status.success() {
            return Ok(None);
        }

        let plist_str = String::from_utf8_lossy(&output.stdout);

        let removable = plist_value_bool(&plist_str, "RemovableMedia");
        let internal = plist_value_bool(&plist_str, "Internal");
        let bus_protocol = plist_value_string(&plist_str, "BusProtocol").unwrap_or_default();
        let name = plist_value_string(&plist_str, "MediaName")
            .or_else(|| plist_value_string(&plist_str, "IORegistryEntryName"))
            .unwrap_or_else(|| "Unknown".to_string());
        let size_bytes = plist_value_u64(&plist_str, "TotalSize").unwrap_or(0);

        if self.verbose {
            eprintln!(
                "[DEBUG] {} — removable={}, internal={}, bus={}, name={}, size={}",
                disk_id, removable, internal, bus_protocol, name, size_bytes
            );
        }

        // Safety checks:
        // - Must have removable media (physically removable card/drive)
        // - Bus type must be USB or Secure Digital
        // - Internal devices are allowed ONLY for Secure Digital (built-in SD readers)
        if !removable {
            return Ok(None);
        }

        let bus_upper = bus_protocol.to_uppercase();
        let is_usb = bus_upper.contains("USB");
        let is_sd = bus_upper.contains("SECURE DIGITAL");

        if !is_usb && !is_sd {
            if self.verbose {
                eprintln!("[DEBUG] Skipping {} — unsupported bus type: {}", disk_id, bus_protocol);
            }
            return Ok(None);
        }

        // Internal USB devices shouldn't exist, but guard against it.
        // Built-in SD card readers are internal — that's expected and safe.
        if internal && !is_sd {
            if self.verbose {
                eprintln!("[DEBUG] Skipping {} — internal non-SD device", disk_id);
            }
            return Ok(None);
        }

        Ok(Some(RemovableDisk {
            identifier: disk_id.to_string(),
            name,
            size_bytes,
            bus_type: bus_protocol,
        }))
    }

    fn print_disk_info(&self, disk: &RemovableDisk) {
        println!("  Device: /dev/{}", disk.identifier);
        println!("  Name:   {}", disk.name);
        println!("  Size:   {}", disk.display_size());
        println!("  Bus:    {}", disk.bus_type);
    }

    fn prompt_disk_selection(&self, count: usize) -> Result<usize> {
        loop {
            print!("Select device [1-{}]: ", count);
            std::io::stdout().flush()?;
            let mut input = String::new();
            std::io::stdin().lock().read_line(&mut input)?;
            if let Ok(n) = input.trim().parse::<usize>() {
                if n >= 1 && n <= count {
                    return Ok(n - 1);
                }
            }
            println!("Invalid selection. Please enter a number between 1 and {}.", count);
        }
    }

    fn confirm(&self, prompt: &str) -> Result<bool> {
        print!("{}", prompt);
        std::io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().lock().read_line(&mut input)?;
        Ok(input.trim().eq_ignore_ascii_case("y"))
    }
}

/// Parse whole-disk identifiers from `diskutil list -plist external` output.
/// Looks for DeviceIdentifier values inside AllDisksAndPartitions array entries
/// that don't contain "s" suffix (partitions like disk2s1).
fn parse_disk_identifiers_from_plist(plist: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut in_device_id = false;

    for line in plist.lines() {
        let trimmed = line.trim();
        if trimmed == "<key>DeviceIdentifier</key>" {
            in_device_id = true;
        } else if in_device_id {
            if let Some(value) = extract_plist_string_value(trimmed) {
                // Only include whole-disk identifiers (e.g., "disk2", not "disk2s1")
                if value.starts_with("disk") && !value.contains('s') {
                    if !ids.contains(&value) {
                        ids.push(value);
                    }
                }
            }
            in_device_id = false;
        }
    }
    ids
}

/// Extract a boolean value for a key from plist XML text.
fn plist_value_bool(plist: &str, key: &str) -> bool {
    let key_tag = format!("<key>{}</key>", key);
    let mut found_key = false;
    for line in plist.lines() {
        let trimmed = line.trim();
        if trimmed == key_tag {
            found_key = true;
        } else if found_key {
            return trimmed == "<true/>";
        }
    }
    false
}

/// Extract a string value for a key from plist XML text.
fn plist_value_string(plist: &str, key: &str) -> Option<String> {
    let key_tag = format!("<key>{}</key>", key);
    let mut found_key = false;
    for line in plist.lines() {
        let trimmed = line.trim();
        if trimmed == key_tag {
            found_key = true;
        } else if found_key {
            return extract_plist_string_value(trimmed);
        }
    }
    None
}

/// Extract a u64 value for a key from plist XML (integer element).
fn plist_value_u64(plist: &str, key: &str) -> Option<u64> {
    let key_tag = format!("<key>{}</key>", key);
    let mut found_key = false;
    for line in plist.lines() {
        let trimmed = line.trim();
        if trimmed == key_tag {
            found_key = true;
        } else if found_key {
            if let Some(stripped) = trimmed.strip_prefix("<integer>") {
                if let Some(num_str) = stripped.strip_suffix("</integer>") {
                    return num_str.parse().ok();
                }
            }
            return None;
        }
    }
    None
}

/// Extract string content from a plist `<string>value</string>` element.
fn extract_plist_string_value(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if let Some(stripped) = trimmed.strip_prefix("<string>") {
        if let Some(value) = stripped.strip_suffix("</string>") {
            return Some(value.to_string());
        }
    }
    None
}

fn format_bytes(bytes: u64) -> String {
    let gb = bytes as f64 / 1_000_000_000.0;
    if gb >= 1.0 {
        format!("{:.1} GB", gb)
    } else {
        let mb = bytes as f64 / 1_000_000.0;
        format!("{:.0} MB", mb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_disk_identifiers() {
        let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>AllDisksAndPartitions</key>
    <array>
        <dict>
            <key>DeviceIdentifier</key>
            <string>disk2</string>
            <key>Partitions</key>
            <array>
                <dict>
                    <key>DeviceIdentifier</key>
                    <string>disk2s1</string>
                </dict>
            </array>
        </dict>
    </array>
</dict>
</plist>"#;

        let ids = parse_disk_identifiers_from_plist(plist);
        assert_eq!(ids, vec!["disk2"]);
    }

    #[test]
    fn test_plist_value_bool() {
        let plist = "    <key>RemovableMedia</key>\n    <true/>\n    <key>Internal</key>\n    <false/>";
        assert!(plist_value_bool(plist, "RemovableMedia"));
        assert!(!plist_value_bool(plist, "Internal"));
        assert!(!plist_value_bool(plist, "NonExistent"));
    }

    #[test]
    fn test_plist_value_string() {
        let plist = "    <key>BusProtocol</key>\n    <string>USB</string>";
        assert_eq!(plist_value_string(plist, "BusProtocol"), Some("USB".to_string()));
        assert_eq!(plist_value_string(plist, "Missing"), None);
    }

    #[test]
    fn test_plist_value_u64() {
        let plist = "    <key>TotalSize</key>\n    <integer>15931539456</integer>";
        assert_eq!(plist_value_u64(plist, "TotalSize"), Some(15931539456));
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(15_931_539_456), "15.9 GB");
        assert_eq!(format_bytes(500_000_000), "500 MB");
    }
}

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
                println!(
                    "  [{}] /dev/{} - {} ({}, {})",
                    i + 1,
                    d.identifier,
                    d.name,
                    d.display_size(),
                    d.bus_type
                );
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
        println!(
            "  Image: {} ({})",
            image_path.file_name().unwrap_or_default().to_string_lossy(),
            format_bytes(image_size)
        );
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
            return Err(anyhow::anyhow!(
                "Failed to unmount /dev/{}",
                disk.identifier
            ));
        }

        // Write image to raw device with progress reporting.
        // We pipe data to dd ourselves so we can track bytes written.
        let raw_device = format!("/dev/r{}", disk.identifier);
        println!("Writing {} to {}...", format_bytes(image_size), raw_device);
        println!("(sudo may prompt for your password)");

        self.write_image_with_progress(image_path, image_size, &raw_device)?;

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
        if let Ok(output) = Command::new("diskutil").args(["list", "-plist"]).output() {
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
                eprintln!(
                    "[DEBUG] Skipping {} — unsupported bus type: {}",
                    disk_id, bus_protocol
                );
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
            println!(
                "Invalid selection. Please enter a number between 1 and {}.",
                count
            );
        }
    }

    fn confirm(&self, prompt: &str) -> Result<bool> {
        print!("{}", prompt);
        std::io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().lock().read_line(&mut input)?;
        Ok(input.trim().eq_ignore_ascii_case("y"))
    }

    /// Write image to raw device with progress reporting.
    /// Pipes data from the image file through to `sudo dd`, tracking bytes
    /// written to display a progress bar.
    fn write_image_with_progress(
        &self,
        image_path: &Path,
        image_size: u64,
        raw_device: &str,
    ) -> Result<()> {
        use std::io::Read as _;
        use std::process::Stdio;

        let file = std::fs::File::open(image_path)?;
        let mut reader = std::io::BufReader::new(file);

        let mut child = Command::new("sudo")
            .args(["dd", &format!("of={}", raw_device), "bs=1m"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("Failed to spawn dd")?;

        let mut stdin = child.stdin.take().context("Failed to open dd stdin")?;
        let mut written: u64 = 0;
        let mut buf = vec![0u8; 1024 * 1024]; // 1 MiB chunks
        let start = std::time::Instant::now();

        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            stdin
                .write_all(&buf[..n])
                .context("Failed to write to dd")?;
            written += n as u64;

            let pct = (written as f64 / image_size as f64) * 100.0;
            let elapsed = start.elapsed().as_secs_f64();
            let speed = if elapsed > 0.0 {
                written as f64 / elapsed / 1_000_000.0
            } else {
                0.0
            };

            print!(
                "\r  {:.1}%  {}/{}  {:.1} MB/s",
                pct,
                format_bytes(written),
                format_bytes(image_size),
                speed
            );
            std::io::stdout().flush()?;
        }

        // Close stdin to signal EOF to dd
        drop(stdin);

        let status = child.wait().context("Failed to wait for dd")?;
        let elapsed = start.elapsed().as_secs_f64();

        println!();
        if !status.success() {
            return Err(anyhow::anyhow!(
                "dd failed with exit code {:?}",
                status.code()
            ));
        }

        println!(
            "  Wrote {} in {:.1}s ({:.1} MB/s)",
            format_bytes(written),
            elapsed,
            written as f64 / elapsed / 1_000_000.0
        );

        Ok(())
    }
}

/// Parse whole-disk identifiers from `diskutil list -plist` output.
/// Looks for the WholeDisks array first (most reliable), then falls back
/// to filtering DeviceIdentifier values to exclude partitions.
fn parse_disk_identifiers_from_plist(plist: &str) -> Vec<String> {
    // Prefer the WholeDisks array — it lists only whole-disk identifiers
    let mut in_whole_disks = false;
    let mut in_array = false;
    let mut ids = Vec::new();

    for line in plist.lines() {
        let trimmed = line.trim();
        if trimmed == "<key>WholeDisks</key>" {
            in_whole_disks = true;
        } else if in_whole_disks && trimmed == "<array>" {
            in_array = true;
        } else if in_array {
            if trimmed == "</array>" {
                break;
            }
            if let Some(value) = extract_plist_string_value(trimmed) {
                if !ids.contains(&value) {
                    ids.push(value);
                }
            }
        }
    }

    if !ids.is_empty() {
        return ids;
    }

    // Fallback: parse DeviceIdentifier values, filtering out partitions.
    // Partitions have an "s" after the disk number (e.g., "disk4s1").
    let mut in_device_id = false;
    for line in plist.lines() {
        let trimmed = line.trim();
        if trimmed == "<key>DeviceIdentifier</key>" {
            in_device_id = true;
        } else if in_device_id {
            if let Some(value) = extract_plist_string_value(trimmed) {
                if value.starts_with("disk") && is_whole_disk_id(&value) && !ids.contains(&value) {
                    ids.push(value);
                }
            }
            in_device_id = false;
        }
    }
    ids
}

/// Check if a disk identifier is a whole disk (e.g., "disk4") vs a partition (e.g., "disk4s1").
/// Partition identifiers have the pattern: disk<N>s<N>
fn is_whole_disk_id(id: &str) -> bool {
    if let Some(after_disk) = id.strip_prefix("disk") {
        // A whole disk id is just digits after "disk" (e.g., "4")
        // A partition has digits, then 's', then more digits (e.g., "4s1")
        after_disk.chars().all(|c| c.is_ascii_digit())
    } else {
        false
    }
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
    fn test_parse_disk_identifiers_from_whole_disks() {
        let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>AllDisksAndPartitions</key>
    <array>
        <dict>
            <key>DeviceIdentifier</key>
            <string>disk4</string>
            <key>Partitions</key>
            <array>
                <dict>
                    <key>DeviceIdentifier</key>
                    <string>disk4s1</string>
                </dict>
            </array>
        </dict>
    </array>
    <key>WholeDisks</key>
    <array>
        <string>disk4</string>
    </array>
</dict>
</plist>"#;

        let ids = parse_disk_identifiers_from_plist(plist);
        assert_eq!(ids, vec!["disk4"]);
    }

    #[test]
    fn test_parse_disk_identifiers_fallback() {
        // No WholeDisks key — falls back to DeviceIdentifier parsing
        let plist = r#"<dict>
    <key>AllDisksAndPartitions</key>
    <array>
        <dict>
            <key>DeviceIdentifier</key>
            <string>disk4</string>
        </dict>
        <dict>
            <key>DeviceIdentifier</key>
            <string>disk4s1</string>
        </dict>
    </array>
</dict>"#;

        let ids = parse_disk_identifiers_from_plist(plist);
        assert_eq!(ids, vec!["disk4"]);
    }

    #[test]
    fn test_is_whole_disk_id() {
        assert!(is_whole_disk_id("disk4"));
        assert!(is_whole_disk_id("disk12"));
        assert!(!is_whole_disk_id("disk4s1"));
        assert!(!is_whole_disk_id("disk4s12"));
        assert!(!is_whole_disk_id("notadisk"));
    }

    #[test]
    fn test_plist_value_bool() {
        let plist =
            "    <key>RemovableMedia</key>\n    <true/>\n    <key>Internal</key>\n    <false/>";
        assert!(plist_value_bool(plist, "RemovableMedia"));
        assert!(!plist_value_bool(plist, "Internal"));
        assert!(!plist_value_bool(plist, "NonExistent"));
    }

    #[test]
    fn test_plist_value_string() {
        let plist = "    <key>BusProtocol</key>\n    <string>USB</string>";
        assert_eq!(
            plist_value_string(plist, "BusProtocol"),
            Some("USB".to_string())
        );
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

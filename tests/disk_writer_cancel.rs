//! The host-side SD card writer must fail when the write is not confirmed.
//!
//! `avocado provision` on Docker Desktop passes the disk image to
//! `DiskWriter::burn_to_removable`. When it returns `Ok`, provision prints
//! success and writes the provision stamp. A write that did not happen must
//! therefore return an error.
//!
//! This test runs the real writer against a fake disk. Stub `diskutil` and
//! `sudo` scripts go first on `PATH`, so the test can never touch a real
//! device. The stubs record each call. The test then checks that the writer
//! did not try to unmount or write the disk.
//!
//! The test changes `PATH` and `AVOCADO_NONINTERACTIVE` for the whole process.
//! Keep it the only test in this file, so that it has its own test binary.

#![cfg(target_os = "macos")]

use avocado_cli::utils::disk_writer::DiskWriter;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const FAKE_DISKUTIL: &str = r#"#!/bin/sh
echo "diskutil $*" >> "$STUB_LOG"
case "$1" in
  list)
    cat <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>WholeDisks</key>
    <array>
        <string>disk99</string>
    </array>
</dict>
</plist>
EOF
    ;;
  info)
    cat <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>BusProtocol</key>
    <string>USB</string>
    <key>Internal</key>
    <false/>
    <key>MediaName</key>
    <string>Fake Test Card</string>
    <key>RemovableMedia</key>
    <true/>
    <key>TotalSize</key>
    <integer>15931539456</integer>
</dict>
</plist>
EOF
    ;;
  *)
    exit 1
    ;;
esac
"#;

const FAKE_SUDO: &str = r#"#!/bin/sh
echo "sudo $*" >> "$STUB_LOG"
exit 1
"#;

fn write_stub(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn burn_without_a_terminal_fails_and_does_not_touch_the_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    write_stub(&bin, "diskutil", FAKE_DISKUTIL);
    write_stub(&bin, "sudo", FAKE_SUDO);

    let log = tmp.path().join("stub.log");
    let image = tmp.path().join("avocado-test.img");
    fs::write(&image, vec![0u8; 4096]).unwrap();

    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    // This is the only test in this binary, so no other thread reads the
    // environment at the same time.
    std::env::set_var("PATH", path);
    std::env::set_var("STUB_LOG", &log);
    std::env::set_var("AVOCADO_NONINTERACTIVE", "1");

    let result = DiskWriter::new(false).burn_to_removable(&image, false);

    let calls = fs::read_to_string(&log).unwrap_or_default();
    assert!(
        calls.contains("diskutil info -plist /dev/disk99"),
        "the writer did not see the fake disk; stub calls:\n{calls}"
    );
    assert!(
        !calls.contains("unmountDisk") && !calls.contains("sudo"),
        "the writer tried to unmount or write the disk without consent; stub calls:\n{calls}"
    );

    let err = result.expect_err("a write without consent must return an error, not Ok");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("no terminal"),
        "the error must say that there was no terminal: {msg}"
    );
    assert!(
        msg.contains("/dev/disk99"),
        "the error must identify the device: {msg}"
    );
}

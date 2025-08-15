#!/usr/bin/env rust-script

use std::fs;
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create a temporary test sysroot structure
    let test_dir = "test-sysroot";

    // Clean up any existing test directory
    if Path::new(test_dir).exists() {
        fs::remove_dir_all(test_dir)?;
    }

    // Create a mock sysroot structure
    fs::create_dir_all(format!("{}/usr/bin", test_dir))?;
    fs::create_dir_all(format!("{}/usr/lib", test_dir))?;
    fs::create_dir_all(format!("{}/etc", test_dir))?;
    fs::create_dir_all(format!("{}/var/log", test_dir))?;

    // Create some sample files
    fs::write(format!("{}/usr/bin/nginx", test_dir), "#!/bin/bash\necho 'nginx mock binary'\n")?;
    fs::write(format!("{}/usr/bin/curl", test_dir), "#!/bin/bash\necho 'curl mock binary'\n")?;
    fs::write(format!("{}/etc/nginx.conf", test_dir), "server { listen 80; }\n")?;
    fs::write(format!("{}/usr/lib/libnginx.so", test_dir), "mock library content")?;
    fs::write(format!("{}/var/log/access.log", test_dir), "127.0.0.1 - - [date] GET / 200\n")?;

    println!("Created test sysroot structure in '{}':", test_dir);

    // List all created files
    for entry in walkdir::WalkDir::new(test_dir) {
        let entry = entry?;
        if entry.file_type().is_file() {
            println!("  {}", entry.path().display());
        }
    }

    println!("\nTest structure created successfully!");
    println!("To test packaging, manually modify the get_sysroot_path function to return");
    println!("the path to this test directory instead of extracting from container.");

    Ok(())
}

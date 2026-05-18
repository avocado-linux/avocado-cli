use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMethod {
    Homebrew,
    Cargo,
    Direct,
}

pub fn detect_install_method(exe: &Path) -> InstallMethod {
    let path_str = exe.to_string_lossy();
    if path_str.contains("/Cellar/avocado-cli/") {
        InstallMethod::Homebrew
    } else if path_str.contains("/.cargo/bin/") {
        InstallMethod::Cargo
    } else {
        InstallMethod::Direct
    }
}

pub fn current_install_method() -> InstallMethod {
    match std::env::current_exe() {
        Ok(path) => detect_install_method(&path),
        Err(_) => InstallMethod::Direct,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn detects_homebrew_apple_silicon() {
        let p = PathBuf::from("/opt/homebrew/Cellar/avocado-cli/0.38.0/bin/avocado");
        assert_eq!(detect_install_method(&p), InstallMethod::Homebrew);
    }

    #[test]
    fn detects_homebrew_intel_mac() {
        let p = PathBuf::from("/usr/local/Cellar/avocado-cli/0.38.0/bin/avocado");
        assert_eq!(detect_install_method(&p), InstallMethod::Homebrew);
    }

    #[test]
    fn detects_homebrew_linux() {
        let p = PathBuf::from("/home/linuxbrew/.linuxbrew/Cellar/avocado-cli/0.38.0/bin/avocado");
        assert_eq!(detect_install_method(&p), InstallMethod::Homebrew);
    }

    #[test]
    fn detects_cargo() {
        let p = PathBuf::from("/Users/x/.cargo/bin/avocado");
        assert_eq!(detect_install_method(&p), InstallMethod::Cargo);
    }

    #[test]
    fn detects_direct() {
        let p = PathBuf::from("/usr/local/bin/avocado");
        assert_eq!(detect_install_method(&p), InstallMethod::Direct);
    }
}

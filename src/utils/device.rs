//! Parses `[user@]host[:port]` device connection strings, shared by
//! `runtime deploy` and `sbom --device` so both reach a device the same way.

use anyhow::Result;

/// Parsed representation of a device connection string.
///
/// Accepts formats: `host`, `user@host`, `host:port`, `user@host:port`
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceSpec {
    pub user: String,
    pub host: String,
    pub port: Option<u16>,
}

impl DeviceSpec {
    pub fn parse(device: &str) -> Result<Self> {
        let (user, host_port) = if let Some(at_pos) = device.find('@') {
            let user = &device[..at_pos];
            anyhow::ensure!(!user.is_empty(), "Empty user in device string '{device}'");
            (user.to_string(), &device[at_pos + 1..])
        } else {
            ("root".to_string(), device)
        };

        anyhow::ensure!(
            !host_port.is_empty(),
            "Empty host in device string '{device}'"
        );

        let (host, port) = if let Some(colon_pos) = host_port.rfind(':') {
            let maybe_port = &host_port[colon_pos + 1..];
            match maybe_port.parse::<u16>() {
                Ok(p) => {
                    let h = &host_port[..colon_pos];
                    anyhow::ensure!(!h.is_empty(), "Empty host in device string '{device}'");
                    (h.to_string(), Some(p))
                }
                // Not a valid port number -- treat the whole thing as a hostname
                // (e.g. IPv6 addresses like ::1)
                Err(_) => (host_port.to_string(), None),
            }
        } else {
            (host_port.to_string(), None)
        };

        Ok(Self { user, host, port })
    }

    /// SSH destination in `user@host` form.
    pub fn ssh_destination(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }

    /// `-p <port>`, or empty if no port was specified.
    pub fn ssh_port_args(&self) -> String {
        match self.port {
            Some(p) => format!("-p {p}"),
            None => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_device_spec_bare_host() {
        let spec = DeviceSpec::parse("192.168.1.100").unwrap();
        assert_eq!(spec.user, "root");
        assert_eq!(spec.host, "192.168.1.100");
        assert_eq!(spec.port, None);
        assert_eq!(spec.ssh_destination(), "root@192.168.1.100");
        assert_eq!(spec.ssh_port_args(), "");
    }

    #[test]
    fn test_device_spec_user_at_host() {
        let spec = DeviceSpec::parse("admin@10.0.0.1").unwrap();
        assert_eq!(spec.user, "admin");
        assert_eq!(spec.host, "10.0.0.1");
        assert_eq!(spec.port, None);
        assert_eq!(spec.ssh_destination(), "admin@10.0.0.1");
    }

    #[test]
    fn test_device_spec_host_with_port() {
        let spec = DeviceSpec::parse("127.0.0.1:2222").unwrap();
        assert_eq!(spec.user, "root");
        assert_eq!(spec.host, "127.0.0.1");
        assert_eq!(spec.port, Some(2222));
        assert_eq!(spec.ssh_destination(), "root@127.0.0.1");
        assert_eq!(spec.ssh_port_args(), "-p 2222");
    }

    #[test]
    fn test_device_spec_user_host_port() {
        let spec = DeviceSpec::parse("root@127.0.0.1:2222").unwrap();
        assert_eq!(spec.user, "root");
        assert_eq!(spec.host, "127.0.0.1");
        assert_eq!(spec.port, Some(2222));
        assert_eq!(spec.ssh_destination(), "root@127.0.0.1");
        assert_eq!(spec.ssh_port_args(), "-p 2222");
    }

    #[test]
    fn test_device_spec_hostname_no_port() {
        let spec = DeviceSpec::parse("device.local").unwrap();
        assert_eq!(spec.user, "root");
        assert_eq!(spec.host, "device.local");
        assert_eq!(spec.port, None);
    }

    #[test]
    fn test_device_spec_hostname_with_port() {
        let spec = DeviceSpec::parse("device.local:22").unwrap();
        assert_eq!(spec.user, "root");
        assert_eq!(spec.host, "device.local");
        assert_eq!(spec.port, Some(22));
    }

    #[test]
    fn test_device_spec_fqdn_user_port() {
        let spec = DeviceSpec::parse("deploy@edge.company.com:2200").unwrap();
        assert_eq!(spec.user, "deploy");
        assert_eq!(spec.host, "edge.company.com");
        assert_eq!(spec.port, Some(2200));
    }

    #[test]
    fn test_device_spec_empty_fails() {
        assert!(DeviceSpec::parse("").is_err());
    }

    #[test]
    fn test_device_spec_empty_user_fails() {
        assert!(DeviceSpec::parse("@host").is_err());
    }
}

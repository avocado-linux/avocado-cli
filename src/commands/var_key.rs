//! `avocado var-key`: the operator side of the /var recovery keyslot.
//!
//! `runtimes.<r>.var.recovery` names a registry secret - the *master*. Nothing
//! derived from it is part of a build: this command talks to one present
//! device, reads its SoC UID, derives HMAC(master, UID) and hands that to
//! `avocadoctl var-key enroll` over the SSH session, which adds the keyslot.
//! Recovering a unit later is `avocado var-key derive` with the same UID on a
//! bench that holds the master. Provision-side by construction: it needs the
//! device, never the image.
use crate::utils::config::Config;
use crate::utils::output::{print_info, print_success, OutputLevel};
use crate::utils::remote::{RemoteHost, SshClient};
use crate::utils::signing_keys::{derive_var_recovery_passphrase, secret_key_bytes};
use anyhow::{Context, Result};

/// How the device reports its SoC UID: the device tree's serial-number (set by
/// the bootloader from the chip id on i.MX and Jetson) first, soc0's
/// serial_number (OCOTP / fuse driver) second - the same order and sources
/// the initramfs var-key.sh uses, so both sides name the same device.
pub const READ_SOC_UID: &str =
    "tr -d '\\0\\n' < /sys/firmware/devicetree/base/serial-number 2>/dev/null \
    || tr -d '\\0\\n' < /sys/devices/soc0/serial_number 2>/dev/null";

/// The token kind avocadoctl records, so a later reader knows what to derive.
pub const DERIVATION_KIND: &str = "hmac-sha256-uid";

fn master_for(config_path: &str, runtime: &str) -> Result<Vec<u8>> {
    let config = Config::load(config_path)?;
    let key = config.get_runtime_var_recovery(runtime).ok_or_else(|| {
        anyhow::anyhow!(
            "runtimes.{runtime}.var.recovery is not set in {config_path}; name a registry secret \
             (`avocado signing-keys create <name> --algorithm hmac-sha256`) to enrol a recovery key"
        )
    })?;
    secret_key_bytes(&key).with_context(|| format!("var.recovery key '{key}'"))
}

pub struct VarKeyEnrollCommand {
    pub config_path: String,
    pub runtime: String,
    pub device: String,
    pub verbose: bool,
}

impl VarKeyEnrollCommand {
    pub async fn execute(&self) -> Result<()> {
        let master = master_for(&self.config_path, &self.runtime)?;
        let remote = RemoteHost::parse(&self.device)?;
        let ssh = SshClient::new(remote).with_verbose(self.verbose);
        ssh.check_connectivity().await?;

        let uid = ssh
            .run_command(READ_SOC_UID)
            .await
            .context("reading the device's SoC UID")?;
        let uid = uid.trim();
        anyhow::ensure!(
            !uid.is_empty(),
            "the device reports no SoC UID (neither /sys/firmware/devicetree/base/serial-number nor \
             /sys/devices/soc0/serial_number); refusing to derive a passphrase that would not be per-device"
        );
        print_info(
            &format!("Device UID {uid}: deriving its recovery passphrase"),
            OutputLevel::Normal,
        );

        let passphrase = derive_var_recovery_passphrase(&master, uid);
        let out = ssh
            .run_command_with_stdin(
                &format!("avocadoctl var-key enroll --kind {DERIVATION_KIND}"),
                &passphrase,
            )
            .await
            .context("avocadoctl var-key enroll on the device")?;
        if self.verbose && !out.trim().is_empty() {
            print_info(out.trim(), OutputLevel::Normal);
        }
        print_success(
            &format!(
                "Recovery keyslot enrolled on {} for runtime '{}'. Recover later with: \
                 avocado var-key derive {} --uid {uid}",
                self.device, self.runtime, self.runtime
            ),
            OutputLevel::Normal,
        );
        Ok(())
    }
}

pub struct VarKeyDeriveCommand {
    pub config_path: String,
    pub runtime: String,
    pub uid: String,
    pub raw: bool,
}

impl VarKeyDeriveCommand {
    /// Prints the passphrase for a device UID: hex by default (paste into
    /// `cryptsetup open --key-file <(xxd -r -p)`), raw bytes with `--raw` for
    /// piping straight into `cryptsetup --key-file -`.
    pub fn execute(&self) -> Result<()> {
        let master = master_for(&self.config_path, &self.runtime)?;
        let passphrase = derive_var_recovery_passphrase(&master, &self.uid);
        if self.raw {
            use std::io::Write;
            std::io::stdout().write_all(&passphrase)?;
        } else {
            println!(
                "{}",
                passphrase
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uid_read_prefers_the_device_tree_then_soc0() {
        let dt = READ_SOC_UID.find("devicetree/base/serial-number").unwrap();
        let soc = READ_SOC_UID.find("soc0/serial_number").unwrap();
        assert!(dt < soc);
        assert!(
            READ_SOC_UID.contains("tr -d"),
            "NULs and newlines stripped so the UID is exact"
        );
    }
}

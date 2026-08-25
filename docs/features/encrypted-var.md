# Encrypted `/var` (`runtimes.<name>.var.encrypt`)

Opt a runtime into a LUKS2-encrypted `/var` whose key is sealed to the
target's hardware key store (the OP-TEE fTPM on Jetson). Off by default; an
unset or `false` value is byte-identical to today's plaintext `/var`.

```yaml
runtimes:
  prod:
    target: jetson-orin-nx
    var:
      encrypt: true
```

## What the cli does when it is set

- Adds `cryptsetup-var` to the initramfs package set and `cryptsetup-var-udev`
  to the rootfs package set (`Config::get_initramfs_packages` /
  `get_rootfs_packages`). The target's BSP makes `cryptsetup-var` pull in
  whatever its key store needs (Jetson: `tpm2-tools`), so the cli stays
  target-agnostic. The initramfs/rootfs sysroots are shared per target, so
  sibling runtimes in the same project receive the package too — dormant
  without the marker below.
- Writes `/etc/avocado/var-encrypt` into **this runtime's** initramfs work
  copy during `runtime build`. That marker is what the initrd keys on.
  `/etc/avocado-security-capabilities` is deliberately left alone: it states
  what the feed's image was built to support and is owned by the feed.
- Nothing changes in `build`'s var image or in `provision`: the plaintext
  btrfs `avocado build` produces (subvolumes, `var_files`, primed images) is
  still flashed. `runtime.<r>.var` is already part of the runtime build
  stamp, so toggling `encrypt` rebuilds.

## What happens on the device

First boot, in the initramfs: the flashed btrfs is encrypted **in place**
(`cryptsetup reencrypt --encrypt`, confined to the seeded bytes), a
device-derived recovery keyslot is created, and a TPM2 keyslot sealed to
PCR 7 is enrolled. Later boots open via the TPM token, falling back to the
recovery slot if the seal breaks (e.g. after a firmware update). Seeded
content survives. Details live in meta-avocado's `cryptsetup-var` recipe.

## Requirements

The target's feed must declare `encrypted-var` in its
`AVOCADO_SECURITY_CAPABILITIES` and publish `cryptsetup-var`; if it does not,
the initrd refuses to touch the partition and `/var` fails to mount rather
than silently staying plaintext. Jetson (orin-nano, orin-nx, agx-orin,
agx-thor) does as of meta-avocado wrynose.

## Limitations

- `encrypt:` is read from the runtime's own `var:` block for package
  selection; placing it only under a `target-<x>:` override is not seen there.
- Device must be re-provisioned to go back to plaintext.

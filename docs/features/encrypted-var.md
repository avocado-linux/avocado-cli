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
  target-agnostic. The initramfs/rootfs sysroots are installed once per
  target, so sibling runtimes **on the same target** receive the package too —
  dormant without the marker below. Which targets a runtime reaches is its
  declared scope: `targets:` (a list) wins, then `target:`, and a runtime that
  declares neither is unscoped and reaches **every** target it is built for.
  An unscoped opt-in therefore does ask a qemu feed for `cryptsetup-var`, and
  fails loudly at install if that feed does not publish it — scope is declared,
  never inferred, so narrowing is the project's decision to state.
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

- A runtime's scope does not choose the build target (`--target` >
  `AVOCADO_TARGET` > `default_target` does). Scope is `targets:` > `target:` >
  unscoped:

  ```yaml
  runtimes:
    dev:
      targets: [jetson-agx-thor, jetson-agx-orin]
      var: { encrypt: true }
  ```

  `default_target` is never consulted — it says what to build when you do not,
  not which targets a runtime belongs to. Building a scoped runtime for a
  target outside its scope fails rather than shipping an initramfs whose
  marker has no `cryptsetup-var` behind it, and an empty `targets: []` is
  rejected outright because it would silently skip every opt-in on the
  runtime. `encrypt:` under a `target-<x>:` override is honored like every
  other `var:` key; an override opting in for a target outside the declared
  scope is an error, not a silent plaintext build.
- Device must be re-provisioned to go back to plaintext.

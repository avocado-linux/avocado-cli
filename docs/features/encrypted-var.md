# Encrypted `/var` (`runtimes.<name>.var`)

Opt a runtime into a LUKS2-encrypted `/var` whose key is sealed to the
target's hardware key store (the OP-TEE fTPM on Jetson). Off by default; an
unset or `false` value is byte-identical to today's plaintext `/var`.

```yaml
runtimes:
  prod:
    target: jetson-orin-nx
    var:
      encrypt: true
      hardware: tpm2
      recovery: var-recovery
```

Three keys, and only the first is required:

| Key | Purpose |
| --- | --- |
| `encrypt` | The opt-in. |
| `hardware` | Which key engine binds the volume. Default `auto`. |
| `recovery` | Names a registry secret held by the operator. |

## `hardware`: which engine, and what happens when it is missing

`auto` (the default), `caam`, `tpm2`, `none`. An unrecognised value is
rejected when the config is parsed, which is worth knowing because `ftpm` is
the obvious guess on Jetson and is not a valid value:

```text
runtimes.prod.var.hardware: 'ftpm' is not one of auto, caam, tpm2, none
```

`auto` uses whatever the machine ships and probes successfully, and **degrades
to Argon2id and reports** when nothing does. The unit boots either way, so a
fleet left on the default can be running software-derived keys while its
operator believes the volume is hardware-bound. `tpm2` and `caam` fail closed
instead. `none` skips the hardware slot entirely and requires `recovery`.

## `recovery`: an operator-held keyslot

Without it, the only way back into a unit whose hardware keyslot is lost is a
keyslot derived from the SoC UID, which is readable on the device. `recovery`
names an HMAC master you hold, and lets that UID-derived slot be retired.

```console
$ avocado signing-keys create var-recovery --algorithm hmac-sha256
```

Nothing derived from the master enters a build. Enrolment happens against a
running device:

```console
$ avocado var-key enroll <runtime> --device root@<host>
```

That reads the device's SoC UID, derives
`HMAC-SHA256(master, "avocado-var-recovery\0" || UID)`, and hands it to
`avocadoctl var-key enroll` over the SSH session. It then re-reads the device's
keyslots and fails unless an `avocado-recovery` token came back, so a device
whose `avocadoctl` is too old to know `var-key` is reported rather than passed.

Recovering a unit later needs only the master and the unit's UID:

```console
$ avocado var-key derive <runtime> --uid <soc-uid>
```

Hex by default; `--raw` emits the 32 bytes for `cryptsetup --key-file -`. The
UID is read from `/sys/firmware/devicetree/base/serial-number`, falling back to
`/sys/devices/soc0/serial_number`.

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

That means the **2026 release, `next` channel**:

```yaml
distro:
  release: 2026
  channel: next
```

`cryptsetup-var` is not published in the 2024 feed at all, so a 2024 project
that sets `encrypt: true` fails during `avocado install` while installing the
rootfs sysroot, and the error names no missing package. An install that dies
there right after the SDK step is the first thing to check against the release.

The 2026 target names also drop the `-devkit` suffix the 2024 feed used
(`jetson-orin-nano`, not `jetson-orin-nano-devkit`), so moving a project
forward is a rename as well as a release bump.

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

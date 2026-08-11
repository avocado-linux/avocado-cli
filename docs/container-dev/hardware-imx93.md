# Container Dev Mode on an NXP FRDM i.MX93

How to get a board to the point where `avocado container dev up` has something to
talk to. The sibling `lab/README.md` covers the QEMU VM path and needs no board;
this one covers real hardware.

Two ways to get an image onto the card. They are not interchangeable, and the
difference is the thing that costs people an afternoon.

## Path A - the CLI (what you want)

```bash
avocado init --target imx93-frdm imx93-frdm
cd imx93-frdm
# declare the runtime in avocado.yaml, then:
avocado install -f
avocado build
avocado provision -r dev --profile sd     # or: --profile uuu-emmc
```

This is the only path that produces a **loginable** device. The CLI composes
extensions onto the base OS and builds the real `/var` that carries them, and it
is where the login credential, the SSH server and the container engine come
from.

Declare at least these extensions, or the board comes up with nothing to talk
to:

```yaml
runtimes:
  dev:
    target: imx93-frdm
    extensions:
      - docker                # the container engine
      - container-agent-dev   # the device agent
      - sshd-dev              # `up` bootstraps the device over SSH
      - my-app                # your app: its image and the unit that runs it
```

`sshd-dev` is the easy miss. A minimal Avocado image ships no SSH server, and
`up` delivers the session bootstrap over SSH before the loop exists.

## Path B - from a local Yocto build

Use this when you need a change that is not in the published feed - a BSP fix, a
kernel option, anything you built yourself.

```bash
# 1. build the images and the provisioning artifacts
bakar bitbake avocado-core  meta-avocado/kas/machine/imx93-frdm.yml
bakar bitbake avocado-stone meta-avocado/kas/machine/imx93-frdm.yml

# 2. assemble the fwup archive. Nothing in bitbake writes this - stone does.
stone provision -i <build>/tmp/deploy/stone --partition-size var=536870912

# 3. write the card
scripts/avocado-flash -m avocado-imx93-frdm sd /dev/sdX
```

Three things that will stop you:

`stone provision` needs `--partition-size var=<bytes>` because the manifest
marks `var` as `expand: true` with no size. Any value above the var image (~110
MB) works; fwup grows the partition to fill the medium at write time, so the
archive only needs a floor.

`stone` shells out to a bare `fwup`, which is not packaged on most hosts. The
build has one under
`tmp/work/*/avocado-stone/*/recipe-sysroot-native/usr/bin/fwup`; it needs that
sysroot's `usr/lib` on `LD_LIBRARY_PATH` to resolve `libconfuse.so.2`. Put a
wrapper on `PATH`. (`avocado-flash` resolves the native tool itself and needs no
wrapper.)

**A build alone does not give you a device you can log into.** A raw Yocto image
is the OS and nothing else: `root` is locked (`root:*:` in `/etc/shadow`), there
is no `authorized_keys`, and there is no `sshd` binary - those arrive with the
extensions the CLI composes. Flashing one and expecting a working board gets you
a console login prompt that no password satisfies. If you need the board usable,
you need Path A's extensions on it, whether or not the OS underneath came from
your own build.

### Updating a board without losing its extensions

`avocado-flash` defaults to fwup's `complete` task, which rewrites the whole
medium **including `/var`** - so it discards the extensions, app data and device
identity already on a provisioned card. To put a new OS onto a working board,
write only the inactive A/B slot instead:

```bash
scripts/avocado-flash -m avocado-imx93-frdm --task upgrade sd /dev/sdX
```

fwup picks `upgrade.a` or `upgrade.b` from `avocado_boot_slot` in the u-boot
environment, so the slot you are running is never the one overwritten and a
failed update leaves the previous system bootable.

## Board specifics

The boot-mode switches are the expensive gotcha: UM12181 Table 12 lists logical
`SW1[3:0]` values while the silkscreen numbers the physical switches 1-4 left to
right, and the two are bit-reverses of each other. Physical positions:

| Mode | Physical switches 1-4 |
| --- | --- |
| SD boot | `1100` (1 and 2 toward ON) |
| eMMC boot | `0100` (2 only) |
| Serial downloader, for `uuu` | `1000` (1 only) |

Setting the switches to `0011` - the obvious reading of Table 12 - selects an
undefined mode and the board does nothing at all, with no console output on
either UART.

Ports: **P1** is power only and carries no data, **P16** is the debug console
(two CDC-ACM devices; the first is the A55), **P2/USB1** is the one `uuu` talks
to. Console is 115200 8N1:

```bash
tio -b 115200 /dev/serial/by-id/usb-1a86_USB_Dual_Serial_<serial>-if00
```

If a board that should be running instead sits at `u-boot=>` and falls through
to a TFTP loop, check which medium it booted rather than assuming the storage
was wiped:

```text
Running BSP bootcmd ...            <- NXP's built-in, not Avocado's
** No partition table - mmc 0 **
Booting from net ...
```

`Running BSP bootcmd` is the tell. Avocado seeds its own `bootcmd` into the
`uboot-env` partition, so seeing NXP's default means u-boot never found that
partition - usually because the switches point at a medium that was never
provisioned.

## Verify it came up

```bash
ssh root@<device-ip> 'systemctl is-active docker.service; docker ps'
```

Then follow the loop steps in the field note or `docs-guides/container-dev-mode`.

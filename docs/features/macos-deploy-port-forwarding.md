# `avocado deploy` on macOS: VM port forwarding

Status: **implemented + validated** (avocado-vm path). Makes
`avocado runtime deploy` work on macOS, where the build/deploy runs
inside the slirp-NAT'd avocado-vm. Validation notes in §12.

## 1. Symptom

`avocado deploy <device>` on macOS fails: the device can't fetch the
TUF repo the deploy serves, so the final
`avocadoctl runtime add --url http://<ip>:8585` step on the device errors
out (connection refused / timeout / wrong host).

## 2. How deploy works (today)

`runtime/deploy.rs` builds a shell script and runs it **inside the SDK
container** (`run_in_container`, `create_deploy_script`):

1. Assembles a TUF repo under `/tmp/avocado-deploy-repo` (metadata +
   symlinked image `.raw` targets).
2. `python3 -m http.server 8585 --bind 0.0.0.0` to serve it.
3. Auto-detects the IP the device should fetch from
   (`ip route get <device>` / `ip -4 addr show scope global`), or honors
   `AVOCADO_DEPLOY_REPO_HOST` if set.
4. SSHes to the device and runs
   `avocadoctl runtime add --url http://<HOST_IP>:8585`.

It already anticipates this problem — the script comment says
`AVOCADO_DEPLOY_REPO_HOST` is "useful for QEMU user-mode networking where
the host is at 10.0.2.2" — but nothing wires it up on macOS.

## 3. Why it breaks on macOS (the topology)

```
macOS host  (LAN: 192.168.x.y, reachable by the device)
   │  qemu, slirp user-mode NAT
   ▼
avocado-vm  (guest 10.0.2.15; host alias 10.0.2.2; NOT inbound-reachable from LAN)
   │  dockerd
   ▼
SDK container  ← the deploy script runs HERE
   • python3 http.server :8585   (bound 0.0.0.0 *inside the container*)
   • ip addr / ip route          → container/VM addresses (docker bridge, 10.0.2.15)
   • ssh → device                (outbound via slirp NAT: OK)
```

Three independent gaps, all from the server living inside the NAT'd VM:

1. **Repo host IP is wrong.** The script's autodetect runs in the
   container and returns a docker-bridge / `10.0.2.15` address. The
   device is handed `http://10.0.2.15:8585`, which is meaningless on the
   LAN. (`10.0.2.2` is only meaningful *inside* the guest, so that's
   wrong too.)
2. **No inbound path to the server.** slirp does not let a LAN device
   reach the guest. Even with the right IP, nothing forwards the device's
   request into the VM/container. The existing qemu `hostfwd` is
   `tcp:127.0.0.1:<port>-:22` — **loopback-only, SSH-only**.
3. **Container port isn't exposed to the VM.** The http.server binds
   `0.0.0.0` *in the container's* netns (docker bridge), not the VM's
   `:8585`. (`deploy.rs` adds no `--net=host` / `-p`, unlike the HITL
   server which does.)

Outbound SSH from the container to the device works (slirp NAT), so the
control path is fine; only the device→repo fetch is broken.

## 4. Proposed design

Reuse the in-container HTTP server (keeps the repo files where they're
staged) and bridge the device→server path with a **per-deploy port
forward** plus the correct repo host. Three pieces:

### 4a. A reusable VM port-forward primitive (QMP)

Add dynamic slirp forwarding via the existing QMP client
([`src/utils/vm/qmp.rs`](../../src/utils/vm/qmp.rs)), using
`human-monitor-command`:

```
hostfwd_add  net0 tcp:0.0.0.0:8585-:8585     # open  (bind 0.0.0.0 → LAN-reachable)
hostfwd_remove net0 tcp:0.0.0.0:8585          # close
```

- `0.0.0.0` (not `127.0.0.1`) so a LAN device can reach `macOS:8585`.
  This is the key difference from the SSH forward.
- Forwards `macOS:8585 → guest 10.0.2.15:8585`.
- Surface it as `avocado vm port-forward add|remove|list <host>:<port>-:<port>`
  for general use, and have deploy call it internally. (A general
  primitive is the "properly support port forwarding" the feature asks
  for; deploy is its first consumer.)
- Alternative: a **static** `hostfwd=tcp:0.0.0.0:8585-:8585` baked into
  `qemu.rs` at VM start. Simpler, but leaves a LAN port open for the
  VM's whole lifetime — rejected in favor of open-only-during-deploy.

### 4b. Expose the container's repo port out of its VM

On macOS (both contexts) publish the repo port from the SDK container so
it escapes the container netns:

- Add `-p 8585:8585` to the deploy container args.
- **avocado-vm:** `-p` publishes onto the VM's interfaces; the qemu
  `hostfwd` (§4a) then carries `macOS:8585 → VM:8585 → container:8585`.
- **Docker Desktop:** `-p` is forwarded straight to the macOS host by
  Docker Desktop's vpnkit — no qemu step.

### 4c. Hand the device the right host IP

Set `AVOCADO_DEPLOY_REPO_HOST` to the macOS host's LAN IP, reusing
[`get_local_ip_for_remote`](../../src/utils/remote.rs) (resolves the
local interface IP that can reach a given device). `deploy.rs` already
forwards `AVOCADO_DEPLOY_REPO_HOST` into the container env — so the
device is told `http://<macOS-LAN-IP>:8585`, which routes back through
the qemu forward.

### End-to-end (external device on the LAN — the primary case)

```
device ── http GET ──► macOS-LAN-IP:8585
                          └─(qemu hostfwd 0.0.0.0:8585→10.0.2.15:8585)
                              └─ VM:8585 ─(docker -p)─► container http.server
container ── ssh ──► device   (outbound slirp NAT)
```

## 5. Detection & orchestration (where the glue lives)

Deploy fails on **both** macOS contexts, because in either one the deploy
container runs inside a Linux VM — the avocado-vm *or* Docker Desktop's
LinuxKit VM — so its in-container `ip route`/`ip addr` autodetect returns
a VM-internal address the device can't reach. Linux runs the container on
native docker with no VM in between, so it already works and must stay
untouched.

So the gate is **two-tier**, mirroring the split the HITL server already
uses (`is_docker_desktop()` → publish vs host-net):

- **`is_docker_desktop()`** (`cfg!(macos) || cfg!(windows)`) — the deploy
  container is inside a Linux VM. Apply the fixes common to both Mac
  contexts:
  1. `AVOCADO_DEPLOY_REPO_HOST` = macOS LAN IP via
     `get_local_ip_for_remote(device_host)` (overrides the broken
     in-container autodetect). Respect an explicit user-set value.
  2. Publish the repo port from the container (`-p <port>:<port>`).
  - **+ `is_vm_routing_active()`** (DOCKER_HOST → avocado-vm socket):
    *also* open a qemu `hostfwd` (`tcp:0.0.0.0:<port>-:<port>`) via QMP
    against the VM's `qmp.sock` (`VmPaths`), because raw slirp doesn't
    auto-expose the published port to the host LAN. Removed on completion
    (success or error); reconcile stale forwards on next `vm start`.
  - **else (Docker Desktop)**: no qemu step — Docker Desktop's `-p`
    already forwards the container port to the macOS host (vpnkit).
- **Linux** — `is_docker_desktop()` false → skip everything; current
  behavior preserved (works today, no VM, native docker).

Key correction over an earlier draft: the discriminator is
`is_docker_desktop()`, **not** `is_vm_routing_active()` alone — the
Docker-Desktop-on-Mac case is broken too. The qemu `hostfwd` is the
*only* avocado-vm-specific piece; the LAN-IP injection + port publish are
shared by both Mac contexts.

## 6. Scenarios

- **External board on the LAN (primary):** fully solved by §4.
- **Deploy to the avocado-vm itself as the device (testing):**
  degenerate — the deploy script runs in a container *inside* the same
  VM and would SSH to the VM and fetch from itself. Out of scope here;
  document that the device must be a reachable address and the VM-as-
  target needs a separate path (e.g. SSH to the bridge gateway), if we
  want it at all.

## 7. Alternatives considered

- **Run the TUF http server on the macOS host** instead of the
  container. No qemu forward needed (host is already on the LAN). But the
  repo targets (manifest + `.raw` images) are staged in the container's
  avocado prefix (an NFS/virtiofs-backed volume in the VM) and symlinked;
  exposing them to a host-side server is more invasive than forwarding a
  port. Rejected for now.
- **Static hostfwd at VM start** (§4a) — simpler, but a permanently-open
  LAN port. Rejected.

## 8. Implementation steps

1. `src/utils/vm/qmp.rs`: add `hostfwd_add` / `hostfwd_remove` /
   `hostfwd_list` helpers (wrap `human-monitor-command`).
2. New `avocado vm port-forward` subcommand (`src/commands/vm/`) using
   those, for general use + tests.
3. `src/commands/runtime/deploy.rs`: on macOS+VM-routing, resolve LAN IP,
   open the forward, inject `AVOCADO_DEPLOY_REPO_HOST` + `-p 8585:8585`,
   and close the forward in a guaranteed-cleanup path.
4. Honor `AVOCADO_DEPLOY_REPO_PORT` end-to-end (forward + publish + URL)
   so a non-default port still works.
5. Tests: QMP command formatting; deploy wiring sets the env + container
   args on macOS and leaves Linux/`--runs-on` paths unchanged.

## 9. Open questions / risks

- **macOS firewall:** opening `0.0.0.0:8585` on the host may prompt the
  application firewall. qemu is the listener; confirm the prompt/behavior.
- **Security:** the forward exposes the repo to the LAN for the deploy's
  duration. Acceptable (TUF metadata is signed; it's transient), but
  document it. Could bind to the specific LAN interface instead of
  `0.0.0.0` if we want to narrow it.
- **Port conflict:** if `8585` is taken on the host, the forward fails —
  pick a free port and thread it through `AVOCADO_DEPLOY_REPO_PORT`.
- **QMP `hostfwd_add` availability:** confirm the bundled qemu's slirp
  build supports runtime `hostfwd_add` (it's standard, but verify on the
  pinned qemu).
- **No desktop change needed:** the CLI owns the qemu lifecycle on macOS;
  the desktop app drives `avocado` and is unaffected. A future Devices/UI
  affordance could call `vm port-forward`, but it's out of scope.

## 12. Validation (2026-06-01)

Validated end-to-end on the **avocado-vm path** deploying to a real
Raspberry Pi 4 on the LAN:

- The repo was served at the **macOS host LAN IP** (`AVOCADO_DEPLOY_REPO_HOST`
  injection working), and the device successfully fetched
  `GET /metadata/timestamp.json → 200` through the qemu `hostfwd` → VM →
  container. The device→repo reachability that was previously impossible
  on macOS now works.
- `qemu hostfwd_add` is accepted by the pinned qemu (open question §9
  resolved).

Findings folded back into the implementation:

- **Host-networking SDK containers:** when the SDK container runs
  `--network=host` (e.g. projects that set it), docker discards the `-p`
  publish with a "Published ports are discarded when using host network
  mode" warning — and it's unnecessary there, since the container already
  shares the VM's `:8585` that the `hostfwd` targets. The shim now skips
  `-p` when host networking is detected.

Out of scope / separate concerns surfaced during testing:

- **Device trust:** sideload deploy then fails at the device with
  `Signature verification failed … got 0, need 1` unless the device's
  installed TUF root matches the project's signing key — i.e. the device
  must be provisioned/flashed from an image built with the same
  `signing-keys`. This is a provisioning/trust matter, independent of the
  port-forwarding fix.
- **Docker Desktop path** (`--no-vm-auto-start`): the LAN-IP + `-p` half
  applies, but it's **not yet validated**, and a project using
  `--network=host` won't expose the port to macOS under Docker Desktop
  (host net there is the LinuxKit VM, not the host) — needs the project
  to use bridge networking, plus confirmation that Docker Desktop's `-p`
  binds a LAN-reachable address.

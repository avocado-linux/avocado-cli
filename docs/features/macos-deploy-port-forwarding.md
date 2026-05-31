# `avocado deploy` on macOS: VM port forwarding

Status: **proposal / plan**. Investigation + design for making
`avocado runtime deploy` work on macOS, where the build/deploy runs
inside the slirp-NAT'd avocado-vm.

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

### 4b. Expose the container's repo port to the VM

In `deploy.rs`, when routing through the VM, publish the repo port from
the SDK container to the VM host so the qemu forward lands on it:

- Add `-p 8585:8585` to the deploy container args (or `--net=host`,
  matching the HITL server's pattern). Then
  `macOS:8585 → (hostfwd) → VM:8585 → (docker -p) → container:8585`.

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

## 5. Orchestration (where the glue lives)

`avocado deploy` runs on the **host**, so the host-side command wraps the
deploy with the forward lifecycle. On macOS + VM-routing active:

1. Resolve macOS LAN IP via `get_local_ip_for_remote(device_host)`.
2. QMP `hostfwd_add net0 tcp:0.0.0.0:8585-:8585` against the VM's
   `qmp.sock` (path from `VmPaths`).
3. Run the deploy container with `AVOCADO_DEPLOY_REPO_HOST=<LAN IP>` and
   `-p 8585:8585`.
4. On completion (success or error), QMP `hostfwd_remove` to close the
   LAN port. Best-effort; also reconcile stale forwards on next `vm
   start`.

Gate all of this on macOS + `route::resolve_mode() == Apply` (the same
signal that says "we're talking to the avocado-vm's docker"). On Linux
with a real local docker / `--runs-on`, deploy already works as-is —
leave it untouched.

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

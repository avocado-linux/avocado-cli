# Container Dev Mode — Phase 0 de-risk findings

Phase 0 is the recorded de-risk gate for the `container-dev-mode` devspec change.
Phase 1 (the embedded registry, watcher, and device agent) is blocked until a GO
is recorded here (task 1.7). Each task below is a spike whose result is recorded,
not production code.

## Status

| Task | What it proves | Status |
|------|----------------|--------|
| 1.1  | Free layer-delta on the device runtime (warm-cache path) | DONE — maintainer-attested in-lab (2026-07-21) |
| 1.2  | Five-host-path sync-latency matrix (macOS split) | DONE — maintainer-attested in-lab (2026-07-21) |
| 1.3  | INGEST digest preservation per image store | DONE — maintainer-attested in-lab (2026-07-21) |
| 1.4  | Native-Linux loopback push + CLI-injected Basic credential, both engines | DONE — docker arm in-session; podman arm maintainer-attested (2026-07-21) |
| 1.5  | Loopback proxy over a production-shaped (TLS+token) bulk leg | DONE — maintainer-attested in-lab (2026-07-21) |
| 1.6  | macOS firewall + non-conflicting default port | DONE — maintainer-attested in-lab (2026-07-21) |
| 1.7  | Recorded GO/NO-GO decision | **GO (2026-07-21)** |
| 1.8  | Authenticated VM push with delivered CA + IP-SAN | DONE — maintainer-attested in-lab (2026-07-21) |
| 1.9  | Agent TLS stack cross-compile across SDK targets | DONE — maintainer-attested in-lab (2026-07-21) |
| 1.10 | Rootless-no-socket podman tag-event emission | DONE — maintainer-attested in-lab (2026-07-21) |
| **1.11** | **Two-socket separation + defense-in-depth auth matrix** | **DONE — in-session (cargo test)** |

## GO/NO-GO decision (task 1.7) — GO, 2026-07-21

**Decision: GO.** Phase 1 (groups 2-8) is unblocked.

Evidence provenance (recorded honestly, per the safety-critical tier):

- **In-session, tool-verified:** 1.11 (axum spike, `cargo test` green + negative-control
  mutation) and the **docker arm of 1.4** (live `docker push` cases: A2 127/8 exemption,
  A10 ephemeral-`DOCKER_CONFIG` credential, H-3 auth-key-must-match).
- **Maintainer-attested in-lab (2026-07-21):** the remaining spikes — 1.1 layer-delta,
  1.2 five-path latency matrix, 1.3 digest preservation, the 1.4 podman arm, 1.5 loopback
  proxy, 1.6 macOS firewall/port, 1.8 VM push, 1.9 cross-compile, 1.10 podman-events — were
  run in the maintainer's lab and confirmed passing. Per-spike measurements are not
  transcribed into this file; the maintainer holds the raw results. These are attested, not
  independently re-verified in-session.

The GO rests on that attestation for 1.1-1.10; the two in-session results stand on their own
tool output above.

## 1.6 — default registry port (recorded for task 2.2)

Task 1.6 chose a non-conflicting default port on stock macOS: **5599**. `5000` is
avoided because the macOS AirPlay Receiver binds it. This is the literal the typed
`container_dev` config uses as `RegistryConfig::DEFAULT_REGISTRY_PORT` when
`registry.port` is omitted (task 2.2), and it matches the loopback registry port
used in the 1.4 spike (`127.0.0.1:5599`).

## 1.4 — Native-Linux loopback push + CLI-injected credential — PARTIAL (docker arm GO)

**Claim under test.** A2 (docker treats a `127.0.0.0/8` registry as trust-free, no cert
config) and A10's docker arm (the CLI supplies the write token via an ephemeral
`DOCKER_CONFIG` forwarded as `X-Registry-Auth`, no persisted `docker login`), plus H-3
(the auth-entry key must be byte-identical to the tagged registry host:port, or docker
omits the credential and the push 401s).

**Setup.** `registry:2` with htpasswd Basic auth (bcrypt, generated via `httpd:2.4-alpine`),
published on `127.0.0.1:5599` (loopback-only) over plain HTTP; `hello-world` tagged
`127.0.0.1:5599/test:dev`; three ephemeral `DOCKER_CONFIG` dirs (matching key, wrong-host
key, empty). Host: docker 29.6.2, no podman.

**Result: docker arm GO.**

| Case | `DOCKER_CONFIG` auth entry | Result |
|------|---------------------------|--------|
| A — anonymous | `{}` (none) | `exit 1`, "no basic auth credentials" — write refused |
| C — wrong host key (H-3) | keyed `localhost:5599`, tag `127.0.0.1:5599` | `exit 1`, "no basic auth credentials" — docker sent NO credential |
| B — matching key (A10) | keyed `127.0.0.1:5599` | `exit 0`, `digest: sha256:c766679d…` pushed |

- **A2 (docker):** case B pushed over plain-HTTP loopback with no `insecure-registries` entry
  and no certs — docker's built-in `127.0.0.0/8` exemption holds.
- **A10 (docker arm):** the Basic credential from an ephemeral `DOCKER_CONFIG` was accepted
  with no `docker login`; nothing was persisted (each push used an isolated `DOCKER_CONFIG`).
- **H-3:** case C proves the auth-entry key must equal the tagged host:port exactly — a
  `localhost` vs `127.0.0.1` mismatch made docker silently omit `X-Registry-Auth`, degrading
  to anonymous → 401. The implementation MUST key the ephemeral auth entry on the exact
  tagged host:port.

**PENDING (podman arm, M-1).** podman is not installed on this host, so the podman side —
`podman push --creds`/`REGISTRY_AUTH_FILE` and, critically, whether podman transmits Basic
over a plaintext loopback under `--tls-verify=false` (M-1) — is unverified. Run on a host
with podman before the overall GO.

**Conclusion.** GO on 1.4's docker arm; 1.4 is NOT complete until the podman arm runs.

## 1.11 — Two-socket separation + defense-in-depth auth matrix — GO

**Claim under test.** The load-bearing security invariant established across cold-review
rounds 3-4: the compromised-device write class is closed *primarily* by route-class =
listener identity (write routes on a loopback-only listener distinct from the
device-reachable bulk read listener, its address never disclosed to a device), with the
Basic/Bearer per-route-class token gate as defense-in-depth. The concern C-1/H-1 raised
was whether this is realizable without depending on registry-middleware per-method
authorization.

**Result: realizable and enforced.** A throwaway `axum` 0.8 crate binds two independent
listeners and enforces the credential-type split. `cargo test` is green; a negative-control
mutation (making the write guard permissive) correctly fails cell 2, so the test is not
vacuous.

Proven:

- **Primary — socket separation.** The write router binds `127.0.0.1:0` and its resolved
  address `is_loopback()`; the read router binds `0.0.0.0:0` and `is_unspecified()`; the two
  addresses differ. Route-class is a listener property, trivially expressible on our own
  axum server — no third-party middleware per-method capability is needed (dissolves the
  round-3 C-1 NO-GO risk).
- **Defense-in-depth — the six auth cells:**

  | Credential | Write route | Read route |
  |-----------|-------------|------------|
  | Basic write token (correct) | 200 accept (cell 1) | 401 refuse (cell 6) |
  | Bearer read/control token | 401 refuse (cell 2) | 200 accept (cell 5) |
  | anonymous (no `Authorization`) | 401 refuse (cell 3) | — |
  | Basic, wrong password | 401 refuse (cell 4) | — |

- **L-1.** The read listener challenges with a bare `WWW-Authenticate: Bearer` (no
  `realm`/token-endpoint redirect that would send a client to a nonexistent auth server).

**Command + output.**

```
$ cargo test --manifest-path /tmp/cdm-1.11-spike/Cargo.toml
running 1 test
test tests::two_socket_separation_and_auth_matrix ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

Negative control (mutation: write guard accepts everything):

```
test tests::two_socket_separation_and_auth_matrix ... FAILED
assertion `left == right` failed: cell 2: Bearer read/control on write route refused
```

**Reproducible source** (throwaway spike; toolchain rustc/cargo 1.97.1, axum 0.8, plain
HTTP — TLS is a separate Phase-0 concern, not this spike's subject):

`Cargo.toml`

```
[package]
name = "cdm-1-11-spike"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
axum = "0.8"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "net"] }
base64 = "0.22"

[dev-dependencies]
reqwest = { version = "0.12", default-features = false }
```

`src/lib.rs` — two `axum::Router`s behind `middleware::from_fn` guards:
`require_basic_write` accepts only Basic `avocado:<write-token>` (else 401 `Basic`);
`require_bearer_read` accepts only `Bearer <read/control-token>` (else 401 bare `Bearer`).
`write_router()` serves `PUT /v2/{name}/manifests/{reference}` + `POST .../blobs/uploads/`;
`read_router()` serves `GET /v2/{name}/manifests/{reference}` + `GET .../blobs/{digest}`.
The test spawns each on an ephemeral port (write on `127.0.0.1:0`, read on `0.0.0.0:0`),
asserts the address properties above, then drives the six cells + the L-1 challenge with
a `reqwest` client.

**Conclusion.** GO on the 1.11 invariant: the two-socket model is realizable on axum and
the per-route-class credential-type gate holds. This de-risks the security model the plan
centers on. It does NOT constitute the overall Phase-0 GO (1.7) — the hardware spikes
(1.1-1.10) remain.

#!/usr/bin/env python3
"""Local implementation of the feed edge contract — seams S2, S3 and S4.

See `avocado-ai/features/secure-feeds/edge-contract.md`. This process stands in
for three production pieces at once so the client half can be built and tested
before any of them exist:

    S2  request authorization   Lambda@Edge ES256/JWKS verifier on /private/*
                                (asymmetric: a CloudFront Function cannot do ECDSA)
    S3  token issuance          Connect POST /api/orgs/:org_id/feed-tokens
    S4  rate limiting           WAF rate-based rules keyed on the User-Agent

It is deliberately NOT a WAF or a CDN. It implements the *observable contract*
those things must satisfy, so the acceptance suite can run unchanged against
either. What it cannot prove is listed in the contract document: distributed
counter semantics, real cache behaviour, shared-address effects, cold starts.

    edge.py --dir FEEDROOT --port N [--pat TOKEN] [--org NAME]
            [--window SECONDS] [--limit-anon N] [--limit-tier N:M ...]

ES256 is used because the production verifier is asymmetric and a symmetric
stand-in would not exercise the same failure modes. A keypair is generated at
startup, plus a *previous* key so token rotation is testable; both are published
at /.well-known/jwks.json.
"""

import argparse
import base64
import collections
import http.server
import json
import os
import sys
import threading
import time
import urllib.parse

try:
    from cryptography.hazmat.primitives import hashes
    from cryptography.hazmat.primitives.asymmetric import ec
    from cryptography.hazmat.primitives.asymmetric.utils import (
        decode_dss_signature,
        encode_dss_signature,
    )
except ImportError:  # pragma: no cover - reported by the caller, not guessed at
    sys.stderr.write(
        "edge.py needs the `cryptography` package for ES256 "
        "(pip install cryptography). The token seams cannot be faked "
        "symmetrically without changing what the test proves.\n"
    )
    raise SystemExit(97)


def b64u(raw: bytes) -> str:
    return base64.urlsafe_b64encode(raw).decode().rstrip("=")


def b64u_dec(txt: str) -> bytes:
    return base64.urlsafe_b64decode(txt + "=" * (-len(txt) % 4))


class Signer:
    """An ES256 signing key, with a predecessor kept for rotation tests."""

    def __init__(self):
        self.current = ec.generate_private_key(ec.SECP256R1())
        self.previous = ec.generate_private_key(ec.SECP256R1())

    def sign(self, claims: dict, *, previous: bool = False) -> str:
        key = self.previous if previous else self.current
        kid = "previous" if previous else "current"
        header = {"alg": "ES256", "typ": "JWT", "kid": kid}
        body = f"{b64u(json.dumps(header).encode())}.{b64u(json.dumps(claims).encode())}"
        der = key.sign(body.encode(), ec.ECDSA(hashes.SHA256()))
        r, s = decode_dss_signature(der)
        raw = r.to_bytes(32, "big") + s.to_bytes(32, "big")
        return f"{body}.{b64u(raw)}"

    def verify(self, token: str):
        """Return the claims, or raise ValueError naming why it was refused.

        Both keys are accepted: rejecting the predecessor would drop tokens that
        were valid when minted, which is the failure a rotation is supposed to
        avoid. Production does the same thing via JWKS carrying both.
        """
        try:
            head_b64, body_b64, sig_b64 = token.split(".")
            claims = json.loads(b64u_dec(body_b64))
            raw = b64u_dec(sig_b64)
            der = encode_dss_signature(
                int.from_bytes(raw[:32], "big"), int.from_bytes(raw[32:], "big")
            )
        except Exception as exc:
            raise ValueError(f"malformed token: {exc}")
        signed = f"{head_b64}.{body_b64}".encode()
        for key in (self.current, self.previous):
            try:
                key.public_key().verify(der, signed, ec.ECDSA(hashes.SHA256()))
                break
            except Exception:
                continue
        else:
            raise ValueError("signature does not verify against any published key")
        if claims.get("exp", 0) <= time.time():
            raise ValueError("token expired")
        return claims

    def jwks(self) -> dict:
        def entry(key, kid):
            nums = key.public_key().public_numbers()
            return {
                "kty": "EC",
                "crv": "P-256",
                "kid": kid,
                "x": b64u(nums.x.to_bytes(32, "big")),
                "y": b64u(nums.y.to_bytes(32, "big")),
            }

        return {"keys": [entry(self.current, "current"), entry(self.previous, "previous")]}


class Limiter:
    """Per-client request counter. Keyed like the WAF rule: the client id from
    the User-Agent when present, the source address when anonymous.

    A fixed window, not a sliding one, and a much shorter one than production —
    the point is to make the *contract* (429 with Retry-After, per-client
    isolation, tier ordering) testable in seconds. Thresholds here mean nothing
    about production thresholds, which come from measured traffic.
    """

    def __init__(self, window: int, limits: dict):
        self.window = window
        self.limits = limits
        self.hits = collections.defaultdict(collections.deque)
        self.lock = threading.Lock()

    def check(self, client: str, tier: int):
        limit = self.limits.get(tier, self.limits[0])
        now = time.time()
        with self.lock:
            seen = self.hits[client]
            while seen and seen[0] <= now - self.window:
                seen.popleft()
            if len(seen) >= limit:
                return int(seen[0] + self.window - now) + 1  # Retry-After
            seen.append(now)
            return None


def parse_ua(ua: str):
    """`avocado-cli/<ver>;key/<id>;tier/<n>` -> (key_id or None, tier).

    Anything unparseable is anonymous at tier 0, which is the safe reading: a
    client that does not identify itself does not get a raised limit.
    """
    key_id, tier = None, 0
    for part in ua.split(";"):
        part = part.strip()
        if part.startswith("key/"):
            key_id = part[4:] or None
        elif part.startswith("tier/"):
            try:
                tier = int(part[5:])
            except ValueError:
                tier = 0
    return key_id, tier


class Handler(http.server.SimpleHTTPRequestHandler):
    signer: Signer = None
    limiter: Limiter = None
    pat = ""
    org = ""
    feed_root = ""
    tier = 1
    max_ttl = 3600

    # --- helpers ---------------------------------------------------------
    def _send(self, code, body=b"", headers=()):
        self.send_response(code)
        for k, v in headers:
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if body and self.command != "HEAD":
            self.wfile.write(body)

    def _deny(self, reason):
        """403 with the reason only in the log, never the body: a client must
        not be able to tell 'wrong token' from 'no such object'."""
        self.denied_reason = reason
        self._send(403, b"forbidden\n")

    # --- S3: token issuance ----------------------------------------------
    def do_POST(self):
        path = urllib.parse.urlparse(self.path).path
        if not path.endswith("/feed-tokens"):
            return self._send(404, b"not found\n")
        if self.headers.get("Authorization", "") != f"Bearer {self.pat}":
            return self._deny("mint: bad account credential")
        org = path.strip("/").split("/")[-2] if "/orgs/" in path else self.org
        if org != self.org:
            return self._deny(f"mint: not a member of {org!r}")
        length = int(self.headers.get("Content-Length") or 0)
        req = json.loads(self.rfile.read(length) or b"{}") if length else {}
        # ttl is a client hint, clamped. `tier` is deliberately NOT read from the
        # request: it is an entitlement derived from the organization, and a client
        # that can ask for a tier can ask for a higher one. It buys only a
        # rate-limit bucket, never access, but it is still a privilege decision
        # taken from client input. Response-only.
        ttl = max(1, min(int(req.get("ttl", 300)), self.max_ttl))
        tier = self.tier
        claims = {
            "org": org,
            "tier": tier,
            "iat": int(time.time()),
            "exp": int(time.time()) + ttl,
        }
        token = self.signer.sign(claims, previous=bool(req.get("previous_key")))
        body = json.dumps(
            {
                "token": token,
                "tier": tier,
                "expires_at": claims["exp"],
                "feed_url": f"http://{self.headers.get('Host')}/private",
            }
        ).encode()
        self._send(200, body, [("Content-Type", "application/json")])

    # --- S2 + S4: authorization and limiting on every read ----------------
    def do_GET(self):
        path = urllib.parse.urlparse(self.path).path
        ua = self.headers.get("User-Agent", "")
        key_id, tier = parse_ua(ua)

        if path == "/.well-known/jwks.json":
            return self._send(
                200, json.dumps(self.signer.jwks()).encode(), [("Content-Type", "application/json")]
            )

        # S4 first, deliberately: production evaluates WAF before the edge
        # function, so a limited request never reaches authorization.
        retry = self.limiter.check(key_id or self.client_address[0], tier)
        if retry is not None:
            self.limited = True
            return self._send(
                429,
                b"rate limited\n",
                [("Retry-After", str(retry)), ("Content-Type", "text/plain")],
            )

        if path.startswith("/private/"):
            auth = self.headers.get("Authorization", "")
            if not auth.startswith("Basic "):
                # 401 with a Basic challenge, NOT 403. dnf/librepo does not send
                # credentials preemptively; it authenticates only after being
                # challenged, and the challenge must say Basic. Answering 403 (or
                # challenging Bearer) makes every object fetch fail outright — the
                # classic "works in curl, dies in dnf". A *bad* credential still
                # gets 403: the client already tried and must not retry.
                self.denied_reason = "no credential (challenged)"
                return self._send(
                    401,
                    b"unauthorized\n",
                    [("WWW-Authenticate", 'Basic realm="avocado-feed"')],
                )
            try:
                _user, _, token = base64.b64decode(auth[6:]).decode().partition(":")
            except Exception:
                return self._deny("undecodable Basic header")
            try:
                claims = self.signer.verify(token)
            except ValueError as exc:
                return self._deny(str(exc))
            # /private/<release>/orgs/<org>/<branch>/<subpath>
            # The org is a path SEGMENT, not a prefix: the release precedes it so
            # the private tree mirrors the public one. Compare the segment after
            # `orgs/`, which is what the production verifier does.
            parts = path.strip("/").split("/")
            try:
                org_at = parts.index("orgs")
                path_org = parts[org_at + 1]
                subpath = "/".join(parts[org_at + 3 :])  # skip org and branch
            except (ValueError, IndexError):
                return self._deny("path is not /private/<rel>/orgs/<org>/<branch>/...")
            if path_org != claims.get("org"):
                return self._deny(
                    f"path org {path_org!r} does not match the token's org {claims.get('org')!r}"
                )
            # Keep the request path for the log: attribution has to record what
            # the client asked for, not the rewritten path the file server sees.
            self.logged_path = path
            self.path = "/" + subpath

        if getattr(self, "head_only", False):
            return super().do_HEAD()
        return super().do_GET()

    def do_HEAD(self):
        # Not an alias for do_GET: the base handler's do_GET always streams the
        # body, so aliasing answers HEAD with a body. Run the same limiting and
        # authorization, then delegate to the real HEAD.
        self.head_only = True
        return self.do_GET()

    # --- S6 (partial): one attributable line per request ------------------
    def log_error(self, *args):
        # BaseHTTPRequestHandler logs errors through both send_error and
        # send_response, which would record a failed request twice and inflate
        # any per-client count derived from this log. One line per request.
        pass

    def log_message(self, fmt, *args):
        key_id, tier = parse_ua(self.headers.get("User-Agent", ""))
        sys.stderr.write(
            "%s %s status=%s key=%s tier=%s%s%s\n"
            % (
                self.command,
                getattr(self, "logged_path", None) or urllib.parse.urlparse(self.path).path,
                args[1] if len(args) > 1 else "-",
                key_id or "-",
                tier,
                " LIMITED" if getattr(self, "limited", False) else "",
                " denied=%s" % getattr(self, "denied_reason", "") if getattr(self, "denied_reason", "") else "",
            )
        )


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument(
        "--dir",
        required=True,
        help="feed root. Objects are served at "
        "/private/<release>/orgs/<org>/<branch>/<subpath>, and everything up to and "
        "including <branch> is stripped when mapping to this directory — so a "
        "target-scoped feed lives at <dir>/target/<target>/repodata/...",
    )
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--pat", default="test-pat", help="the account credential the mint accepts")
    ap.add_argument("--org", default="acme")
    ap.add_argument("--tier", type=int, default=1, help="the org's entitlement, server-side")
    ap.add_argument("--max-ttl", type=int, default=3600)
    ap.add_argument("--window", type=int, default=10, help="rate-limit window, seconds")
    ap.add_argument("--limit-anon", type=int, default=5)
    ap.add_argument(
        "--limit-tier",
        action="append",
        default=[],
        metavar="TIER:N",
        help="per-tier request ceiling, e.g. 1:20",
    )
    args = ap.parse_args()

    limits = {0: args.limit_anon}
    for spec in args.limit_tier:
        tier, _, n = spec.partition(":")
        limits[int(tier)] = int(n)
    limits.setdefault(1, args.limit_anon * 4)

    Handler.signer = Signer()
    Handler.limiter = Limiter(args.window, limits)
    Handler.pat = args.pat
    Handler.org = args.org
    Handler.tier = args.tier
    Handler.max_ttl = args.max_ttl
    Handler.feed_root = os.path.abspath(args.dir)

    def build(*a, **kw):
        return Handler(*a, directory=Handler.feed_root, **kw)

    srv = http.server.ThreadingHTTPServer(("0.0.0.0", args.port), build)
    sys.stderr.write(
        f"edge: dir={Handler.feed_root} org={args.org} window={args.window}s limits={limits}\n"
    )
    srv.serve_forever()


if __name__ == "__main__":
    main()

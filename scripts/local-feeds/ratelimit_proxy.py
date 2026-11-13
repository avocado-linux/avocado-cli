#!/usr/bin/env python3
"""A rate-limiting proxy in front of a real package feed, for testing clients.

Answers 429 with Retry-After once a counter passes its limit, with rules of the
kind CDNs commonly offer, evaluated in this order:

    BuildServer   per address, only for --build-ip addresses        --limit-build
    Tier<n>       per whole User-Agent, when it carries tier/<n>     --limit-tier n:M
    IpBackstop    per address, every other request                   --limit-backstop
    Global        per address; with --enforce, only requests that
                  carry no configured tier/<n>                       --limit-anon

Without --enforce, Tier and IpBackstop only count and Global answers a bare 403,
so both answers can be tested. Each request is logged as one JSON line with the
rule that matched, so a run can be graded afterwards.

Counters use a sliding window (--window seconds). This proves client behaviour,
not thresholds.

    ratelimit_proxy.py --upstream https://repo.avocadolinux.org --port 8099 \
        [--enforce] [--window 300] [--limit-anon N] [--limit-tier 1:N ...] \
        [--limit-backstop N] [--build-ip ADDR ...] [--log FILE]
    ratelimit_proxy.py --self-test
"""

import argparse
import collections
import http.server
import json
import re
import sys
import threading
import time
import urllib.error
import urllib.request

PASS_HEADERS = ("content-type", "content-length", "etag", "last-modified",
                "cache-control", "accept-ranges", "content-range")


class Limiter:
    def __init__(self, window):
        self.window = window
        self.hits = collections.defaultdict(collections.deque)
        self.lock = threading.Lock()

    def hit(self, key, limit):
        """Count one request; return seconds to wait if over limit, else 0."""
        now = time.monotonic()
        with self.lock:
            q = self.hits[key]
            while q and q[0] <= now - self.window:
                q.popleft()
            q.append(now)
            if len(q) > limit:
                return max(1, int(q[0] + self.window - now) + 1)
            return 0


def rules(a, ip, ua):
    """Yield (rule name, counter key, limit, enforcing?) in priority order."""
    build = ip in a.build_ip
    if build:
        yield "BuildServer", ("b", ip), a.limit_build, True
    for tier, limit in sorted(a.limit_tier.items()):
        if f"tier/{tier}" in ua:
            yield f"Tier{tier}", ("t", tier, ua), limit, a.enforce
    if not build:
        yield "IpBackstop", ("ip", ip), a.limit_backstop, a.enforce
        tiered = a.limit_tier and re.search(
            "tier/(" + "|".join(sorted(a.limit_tier)) + ")", ua)
        if not (a.enforce and tiered):
            yield "Global", ("g", ip), a.limit_anon, True


def make_handler(a, limiter, log):
    class H(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_):
            pass

        def record(self, status, rule, ua):
            line = json.dumps({"t": round(time.time(), 3), "ip": self.client_address[0],
                               "ua": ua, "method": self.command, "path": self.path,
                               "status": status, "rule": rule})
            with lock:
                log.write(line + "\n")
                log.flush()

        def deny(self, status, retry, rule, ua):
            body = b"rate limited\n" if status == 429 else b"forbidden\n"
            self.send_response(status)
            if status == 429:
                self.send_header("Retry-After", str(a.retry_after or retry))
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(body)
            self.record(status, rule, ua)

        def do_GET(self):
            ua = self.headers.get("User-Agent", "")
            ip = self.client_address[0]
            for name, key, limit, enforcing in rules(a, ip, ua):
                wait = limiter.hit(key, limit)
                if wait and enforcing:
                    status = 429 if (a.enforce or name != "Global") else 403
                    return self.deny(status, wait, name, ua)
            req = urllib.request.Request(a.upstream + self.path, method=self.command,
                                         headers={"User-Agent": ua, **({"Range": self.headers["Range"]} if self.headers.get("Range") else {})})
            try:
                up = urllib.request.urlopen(req, timeout=60)
            except urllib.error.HTTPError as e:
                up = e
            except Exception as e:  # upstream unreachable: a real 5xx
                self.send_response(502)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return self.record(502, f"upstream:{e}", ua)
            self.send_response(up.status if hasattr(up, "status") else up.code)
            for h in PASS_HEADERS:
                if up.headers.get(h):
                    self.send_header(h, up.headers[h])
            if not up.headers.get("content-length"):
                self.send_header("Connection", "close")
                self.close_connection = True
            self.end_headers()
            if self.command != "HEAD":
                while chunk := up.read(1 << 16):
                    self.wfile.write(chunk)
            self.record(up.status if hasattr(up, "status") else up.code, "allow", ua)

        do_HEAD = do_GET

    lock = threading.Lock()
    return H


def self_test():
    """`ratelimit_proxy.py --self-test`: the rule table."""
    def args(enforce):
        return argparse.Namespace(enforce=enforce, build_ip=["192.0.2.10"], limit_build=100,
                                  limit_tier={"1": 20, "3": 30}, limit_backstop=50, limit_anon=10)

    def names(a, ip, ua):
        return [(n, e) for n, _, _, e in rules(a, ip, ua)]

    count, enf = args(False), args(True)
    anon, t1 = "avocado-cli/1.1.0", "avocado-cli/1.1.0;key/abc;tier/1"
    # Build servers: only their own rule, never the anonymous one.
    assert names(count, "192.0.2.10", anon) == [("BuildServer", True)]
    # Count mode: tier and backstop count only; anonymous still blocks, tiered or not.
    assert names(count, "198.51.100.1", t1) == [("Tier1", False), ("IpBackstop", False), ("Global", True)]
    assert names(count, "198.51.100.1", anon) == [("IpBackstop", False), ("Global", True)]
    # Enforcing: a configured tier leaves the anonymous rule; an unknown tier does not.
    assert names(enf, "198.51.100.1", t1) == [("Tier1", True), ("IpBackstop", True)]
    assert names(enf, "198.51.100.1", "x;key/a;tier/2") == [("IpBackstop", True), ("Global", True)]
    # Counters are per whole User-Agent for tiers, per address otherwise.
    keys = {k for _, k, _, _ in rules(enf, "198.51.100.1", t1)}
    assert ("t", "1", t1) in keys and ("ip", "198.51.100.1") in keys
    # The limiter: over the limit inside the window, a wait inside the window.
    lim = Limiter(window=60)
    assert [lim.hit("k", 2) for _ in range(2)] == [0, 0]
    wait = lim.hit("k", 2)
    assert 1 <= wait <= 61, wait
    assert lim.hit("other", 2) == 0
    print("ratelimit_proxy.py self-test: ok")


def main():
    if "--self-test" in sys.argv:
        return self_test()
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("--upstream", required=True)
    p.add_argument("--host", default="0.0.0.0")
    p.add_argument("--port", type=int, default=8099)
    p.add_argument("--enforce", action="store_true")
    p.add_argument("--window", type=float, default=300)
    p.add_argument("--retry-after", type=int, default=0,
                   help="fixed Retry-After (production sends the window, 300)")
    p.add_argument("--limit-anon", type=int, default=20000)
    p.add_argument("--limit-backstop", type=int, default=100000)
    p.add_argument("--limit-build", type=int, default=100000)
    p.add_argument("--limit-tier", action="append", default=[], metavar="N:M")
    p.add_argument("--build-ip", action="append", default=[])
    p.add_argument("--log", default="-")
    a = p.parse_args()
    a.upstream = a.upstream.rstrip("/")
    a.limit_tier = {k: int(v) for k, v in (x.split(":") for x in a.limit_tier)}
    log = sys.stdout if a.log == "-" else open(a.log, "a")
    srv = http.server.ThreadingHTTPServer((a.host, a.port), make_handler(a, Limiter(a.window), log))
    print(f"ratelimit_proxy.py on :{a.port} -> {a.upstream} enforce={a.enforce} window={a.window}s "
          f"anon={a.limit_anon} tiers={a.limit_tier} backstop={a.limit_backstop}", file=sys.stderr, flush=True)
    srv.serve_forever()


if __name__ == "__main__":
    main()

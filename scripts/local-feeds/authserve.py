#!/usr/bin/env python3
"""Static file server with HTTP Basic auth — a stand-in for a private feed.

    authserve.py <dir> <port> <user> <password>

Every request must carry `Authorization: Basic base64(user:password)`; anything
else gets 401. Logs one line per request (method, path, status, User-Agent) so
the test can also see what dnf actually sent. Stdlib only.
"""
import base64
import http.server
import sys
from functools import partial


class AuthHandler(http.server.SimpleHTTPRequestHandler):
    expected = ""

    def _authorized(self):
        return self.headers.get("Authorization", "") == self.expected

    def do_GET(self):
        if not self._authorized():
            self.send_response(401)
            self.send_header("WWW-Authenticate", 'Basic realm="feed"')
            self.end_headers()
            return
        super().do_GET()

    do_HEAD = do_GET

    def log_message(self, fmt, *args):
        sys.stderr.write(
            "%s %s %s ua=%r\n"
            % (self.command, self.path, args[1] if len(args) > 1 else "-", self.headers.get("User-Agent", ""))
        )


def main():
    directory, port, user, password = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
    AuthHandler.expected = "Basic " + base64.b64encode(f"{user}:{password}".encode()).decode()
    handler = partial(AuthHandler, directory=directory)
    http.server.ThreadingHTTPServer(("0.0.0.0", port), handler).serve_forever()


if __name__ == "__main__":
    main()

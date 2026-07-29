#!/usr/bin/env python3
"""Captures a screenshot of every dashboard view, empty and populated.

The dashboard is server-rendered HTML with no component library and no story
book, so the only way to see what a page actually looks like is to run it. That
makes visual regressions cheap to introduce and expensive to notice: a card that
collapses, a table that overflows, an empty state that reads as an error. This
script renders the lot in one command so they can be looked at.

Every view is captured twice, because the two states fail differently:

* **empty** — a fresh instance with nothing configured. This is what a new
  operator sees first, and it is the state where a missing empty-state message
  looks like a broken page.
* **populated** — targets, build hosts, services, secrets and a deployment,
  including the states that only appear when something is wrong: a
  latency-critical host, a build host that is the instance default, a service
  pinned to a build host.

Both light and dark are captured, since the stylesheet supports both and only
one of them is ever looked at during development.

    scripts/screenshots.py                  # start a demo, capture, tear down
    scripts/screenshots.py --keep           # leave the instance running
    scripts/screenshots.py --only build     # views whose name contains "build"
    scripts/screenshots.py --url http://…   # capture an instance already running

Needs Docker (for the instance) and Google Chrome (for the rendering). Output
lands in `screenshots/` — gitignored, since these are for looking at rather than
committing.

No third-party packages: it speaks the Chrome DevTools Protocol over a
websocket implemented against the standard library, so it runs anywhere the rest
of the tooling does without a virtualenv.
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import pathlib
import re
import secrets
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "screenshots"

CONTAINER = "nudo-screenshots"
WEB_PORT = 3100
CHROME_PORT = 9333

EMAIL = "screenshots@nudo.test"
PASSWORD = "screenshots-password-123"

CHROME_CANDIDATES = [
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    shutil.which("google-chrome") or "",
    shutil.which("chromium") or "",
    shutil.which("chromium-browser") or "",
]


# ---------------------------------------------------------------------------
# The views
# ---------------------------------------------------------------------------

# name -> path. Ids are filled in from what the seeding step created, so a view
# that needs one is written as a format string.
VIEWS: list[tuple[str, str]] = [
    ("dashboard", "/"),
    ("targets-list", "/targets"),
    ("targets-new", "/targets/new"),
    ("target-detail", "/targets/{target_id}"),
    ("target-ingress-config", "/targets/{target_id}/ingress/config"),
    ("build-hosts-list", "/build-hosts"),
    ("build-hosts-new", "/build-hosts/new"),
    ("build-host-detail", "/build-hosts/{build_host_id}"),
    ("build-host-detail-latency-critical", "/build-hosts/{hot_build_host_id}"),
    ("services-list", "/services"),
    ("services-new", "/services/new"),
    ("service-detail", "/services/{service_id}"),
    ("service-edit", "/services/{service_id}/edit"),
    ("deployments", "/deployments"),
    ("secrets", "/secrets"),
    ("sources", "/sources"),
    ("audit", "/audit"),
    ("settings", "/settings"),
    ("terminal", "/terminal"),
    ("login", "/login"),
]

# Views that only make sense once something exists. Skipped in the empty state
# rather than captured as a 404, which would be noise rather than information.
NEEDS_DATA = {
    "target-detail",
    "target-ingress-config",
    "build-host-detail",
    "build-host-detail-latency-critical",
    "service-detail",
    "service-edit",
}


# ---------------------------------------------------------------------------
# A minimal Chrome DevTools Protocol client
# ---------------------------------------------------------------------------


class WebSocket:
    """Just enough of RFC 6455 to talk to Chrome.

    Chrome is on loopback and speaks only what it is asked to, so this omits
    everything the protocol allows but Chrome never does: fragmentation,
    extensions, and server-to-client masking.
    """

    def __init__(self, url: str) -> None:
        parsed = urllib.parse.urlparse(url)
        self.sock = socket.create_connection((parsed.hostname, parsed.port), 10)
        self.sock.settimeout(30)

        key = base64.b64encode(secrets.token_bytes(16)).decode()
        path = parsed.path or "/"
        if parsed.query:
            path += "?" + parsed.query

        self.sock.sendall(
            (
                f"GET {path} HTTP/1.1\r\n"
                f"Host: {parsed.hostname}:{parsed.port}\r\n"
                "Upgrade: websocket\r\n"
                "Connection: Upgrade\r\n"
                f"Sec-WebSocket-Key: {key}\r\n"
                "Sec-WebSocket-Version: 13\r\n\r\n"
            ).encode()
        )

        # Read past the handshake response.
        buf = b""
        while b"\r\n\r\n" not in buf:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise RuntimeError("chrome closed the connection during the handshake")
            buf += chunk
        if b"101" not in buf.split(b"\r\n")[0]:
            raise RuntimeError(f"chrome refused the websocket upgrade: {buf[:200]!r}")
        self.buffer = buf.split(b"\r\n\r\n", 1)[1]

    def send(self, payload: str) -> None:
        data = payload.encode()
        header = bytearray([0x81])  # FIN + text
        mask = secrets.token_bytes(4)
        length = len(data)
        if length < 126:
            header.append(0x80 | length)
        elif length < (1 << 16):
            header.append(0x80 | 126)
            header += struct.pack(">H", length)
        else:
            header.append(0x80 | 127)
            header += struct.pack(">Q", length)
        header += mask
        masked = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
        self.sock.sendall(bytes(header) + masked)

    def _read(self, count: int) -> bytes:
        while len(self.buffer) < count:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise RuntimeError("chrome closed the connection")
            self.buffer += chunk
        out, self.buffer = self.buffer[:count], self.buffer[count:]
        return out

    def recv(self) -> str:
        while True:
            first, second = self._read(2)
            opcode = first & 0x0F
            length = second & 0x7F
            if length == 126:
                length = struct.unpack(">H", self._read(2))[0]
            elif length == 127:
                length = struct.unpack(">Q", self._read(8))[0]
            payload = self._read(length)

            if opcode == 0x8:  # close
                raise RuntimeError("chrome closed the websocket")
            if opcode == 0x9:  # ping -> pong
                self.sock.sendall(b"\x8a\x80" + secrets.token_bytes(4))
                continue
            if opcode in (0x1, 0x2):
                return payload.decode("utf-8", "replace")
            # Anything else (pong) is ignored.

    def close(self) -> None:
        try:
            self.sock.close()
        except OSError:
            pass


class Chrome:
    """A headless Chrome, driven over CDP."""

    def __init__(self, binary: str, width: int, height: int) -> None:
        self.profile = tempfile.mkdtemp(prefix="nudo-shots-")
        self.proc = subprocess.Popen(
            [
                binary,
                "--headless=new",
                "--disable-gpu",
                "--hide-scrollbars",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-extensions",
                f"--remote-debugging-port={CHROME_PORT}",
                f"--user-data-dir={self.profile}",
                f"--window-size={width},{height}",
                "about:blank",
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

        target = wait_for(
            "chrome to expose a debugging target",
            60,
            self._first_page_target,
        )
        self.ws = WebSocket(target)
        self.next_id = 0
        self.call("Page.enable")
        self.call("Network.enable")

    def _first_page_target(self):
        try:
            with urllib.request.urlopen(
                f"http://127.0.0.1:{CHROME_PORT}/json", timeout=2
            ) as response:
                for entry in json.load(response):
                    if entry.get("type") == "page":
                        return entry["webSocketDebuggerUrl"]
        except (urllib.error.URLError, OSError, json.JSONDecodeError, KeyError):
            return None
        return None

    def call(self, method: str, **params) -> dict:
        self.next_id += 1
        message_id = self.next_id
        self.ws.send(json.dumps({"id": message_id, "method": method, "params": params}))
        while True:
            message = json.loads(self.ws.recv())
            if message.get("id") != message_id:
                continue  # an event, or another call's reply
            if "error" in message:
                raise RuntimeError(f"{method} failed: {message['error']}")
            return message.get("result", {})

    def set_cookie(self, name: str, value: str, url: str) -> None:
        self.call("Network.setCookie", name=name, value=value, url=url)

    def set_theme(self, dark: bool) -> None:
        # The stylesheet keys off prefers-color-scheme, so this is what decides
        # which of the two designs is rendered.
        self.call(
            "Emulation.setEmulatedMedia",
            features=[{"name": "prefers-color-scheme", "value": "dark" if dark else "light"}],
        )

    def capture(self, url: str, path: pathlib.Path) -> None:
        self.call("Page.navigate", url=url)
        # A fixed settle rather than waiting on a load event: the pages that
        # matter here stream (deployments, logs) and never reach quiescence.
        time.sleep(1.2)

        metrics = self.call("Page.getLayoutMetrics")
        content = metrics.get("cssContentSize") or metrics["contentSize"]
        width = max(int(content["width"]), 1200)

        # How far down the page the *content* is actually drawn.
        #
        # Two things are sized to the viewport rather than to what they contain:
        # `.shell` is `min-height: 100vh` and `.rail` is `height: 100vh`. So the
        # document reports a full viewport however little is on it, and a
        # two-row table screenshots with two-thirds empty grey below it.
        #
        # Every container is stretched: `.shell` is `min-height: 100vh`, `.rail`
        # is `height: 100vh`, and `.main`/`.content` are `flex: 1` inside it. So
        # measuring any of them just reports the viewport back, and a two-row
        # table screenshots with two-thirds empty grey below it.
        #
        # The direct children of `.topbar` and `.content` are the things
        # actually sized by what they contain, so the bottom of the last one is
        # where the page really ends. The viewport override below then stretches
        # the containers — including the rail — to exactly that.
        bottom = self.call(
            "Runtime.evaluate",
            expression="""
                (() => {
                    let content = 0;
                    for (const section of document.querySelectorAll(".topbar, .content")) {
                        for (const node of section.children) {
                            const box = node.getBoundingClientRect();
                            if (box.height === 0 && box.width === 0) continue;
                            content = Math.max(content, box.bottom + window.scrollY);
                        }
                    }

                    // The rail is `height: 100vh` and a `.spacer` pushes its
                    // footer to the bottom, so neither is a fixed measure. The
                    // last nav link is: it sits where the navigation naturally
                    // ends. Cropping above it would cut the nav, which is worse
                    // than a little empty space beside a short page.
                    let rail = 0;
                    const links = document.querySelectorAll(".rail a.nav");
                    if (links.length) {
                        const last = links[links.length - 1];
                        rail = last.getBoundingClientRect().bottom + window.scrollY;
                    }

                    return Math.ceil(Math.max(content, rail));
                })()
            """,
            returnByValue=True,
        )["result"].get("value", 0)

        if not bottom:
            # A page with neither section — the login screen centres its card in
            # the viewport — keeps whatever the document reports.
            bottom = int(content["height"])

        # A little breathing room under the last element, and bounded: a log
        # view can be tens of thousands of pixels tall, which is not something
        # anybody is going to look at.
        height = min(max(int(bottom) + 24, 320), 4000)

        # Resize the viewport to the page before capturing. The rail is
        # `position: fixed` and sized to the viewport, so capturing a tall page
        # from a short viewport leaves it ending partway down with empty space
        # beside the content — the nav appears cut off in every long screenshot.
        self.call(
            "Emulation.setDeviceMetricsOverride",
            width=width,
            height=height,
            deviceScaleFactor=1,
            mobile=False,
        )
        try:
            result = self.call(
                "Page.captureScreenshot",
                format="png",
                captureBeyondViewport=True,
                clip={"x": 0, "y": 0, "width": width, "height": height, "scale": 1},
            )
        finally:
            self.call("Emulation.clearDeviceMetricsOverride")
        path.write_bytes(base64.b64decode(result["data"]))

    def close(self) -> None:
        try:
            self.ws.close()
        finally:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
            shutil.rmtree(self.profile, ignore_errors=True)


# ---------------------------------------------------------------------------
# The instance
# ---------------------------------------------------------------------------


class Instance:
    """A throwaway nudo, in Docker."""

    def __init__(self, image: str) -> None:
        self.url = f"http://127.0.0.1:{WEB_PORT}"
        self.image = image

    def start(self) -> None:
        subprocess.run(
            ["docker", "rm", "-f", CONTAINER],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        key = secrets.token_hex(32)
        run(
            [
                "docker", "run", "-d", "--name", CONTAINER,
                "-p", f"{WEB_PORT}:3000",
                "-e", f"NUDO_SECRET_KEY={key}",
                "-e", f"NUDO_BASE_URL={self.url}",
                self.image,
            ]
        )
        wait_for("nudo to serve the dashboard", 120, self._serving)

    def _serving(self):
        try:
            with urllib.request.urlopen(f"{self.url}/login", timeout=2) as response:
                return response.status == 200 or None
        except (urllib.error.URLError, OSError):
            return None

    def stop(self) -> None:
        subprocess.run(
            ["docker", "rm", "-f", CONTAINER],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )


class _NoRedirects(urllib.request.HTTPRedirectHandler):
    """Turns a redirect into an `HTTPError` instead of following it."""

    def redirect_request(self, *args, **kwargs):
        return None


class Session:
    """An authenticated dashboard session, driven over HTTP."""

    def __init__(self, base: str) -> None:
        self.base = base.rstrip("/")
        self.cookie = ""
        self.opener = urllib.request.build_opener(_NoRedirects)

    def get(self, path: str) -> str:
        request = urllib.request.Request(self.base + path)
        if self.cookie:
            request.add_header("Cookie", self.cookie)
        with urllib.request.urlopen(request, timeout=30) as response:
            return response.read().decode("utf-8", "replace")

    def post(self, path: str, fields: dict[str, str]) -> str:
        """Posts a form and returns the `Location` it redirected to.

        Redirects are deliberately *not* followed. Every mutating handler in the
        dashboard answers 303, and both things this needs — the session cookie
        and the id of what was just created — are on that response. Letting
        urllib follow it would hand back the redirect target's headers, where
        neither exists.
        """
        body = urllib.parse.urlencode(fields).encode()
        request = urllib.request.Request(self.base + path, data=body, method="POST")
        request.add_header("Content-Type", "application/x-www-form-urlencoded")
        if self.cookie:
            request.add_header("Cookie", self.cookie)

        try:
            with self.opener.open(request, timeout=60) as response:
                self._remember(response)
                return response.headers.get("Location", "")
        except urllib.error.HTTPError as error:
            # A 3xx arrives here because the opener refuses to follow it.
            if error.status in (301, 302, 303, 307, 308):
                self._remember(error)
                return error.headers.get("Location", "")
            raise

    def _remember(self, response) -> None:
        for header in response.headers.get_all("Set-Cookie") or []:
            if header.startswith("nudo_session="):
                self.cookie = header.split(";", 1)[0]

    def csrf(self, path: str) -> str:
        match = re.search(r'name="csrf" value="([^"]*)"', self.get(path))
        if not match:
            raise RuntimeError(f"no csrf token on {path}")
        return match.group(1)

    def session_id(self) -> str:
        return self.cookie.split("=", 1)[1] if "=" in self.cookie else ""


def sign_in_at(base: str) -> Session:
    """Creates the first account, or signs in if one already exists.

    `/login` serves both forms — first-run setup on a fresh instance, sign-in
    once an account exists — and they post to different paths with different
    tokens. Which one is rendered is the only reliable signal, so it is read
    from the form rather than guessed at by trying one and falling back: a
    wasted POST would burn the CSRF token the second attempt needs.
    """
    session = Session(base)
    page = session.get("/login")

    match = re.search(r'name="csrf" value="([^"]*)"', page)
    if not match:
        raise RuntimeError("no csrf token on /login")
    csrf = match.group(1)

    setting_up = 'name="password_confirm"' in page
    if setting_up:
        session.post(
            "/setup",
            {
                "csrf": csrf,
                "email": EMAIL,
                "password": PASSWORD,
                "password_confirm": PASSWORD,
            },
        )
    else:
        session.post("/login", {"csrf": csrf, "email": EMAIL, "password": PASSWORD})

    if not session.cookie:
        what = "create the first account" if setting_up else "sign in"
        raise RuntimeError(
            f"could not {what} at {base}. If this instance already has accounts, "
            f"either sign in as {EMAIL} or point --url at a fresh one."
        )
    return session


# ---------------------------------------------------------------------------
# Seeding
# ---------------------------------------------------------------------------


def seed(session: Session) -> dict[str, str]:
    """Creates enough for every populated view to have something to show.

    Nothing here has to be reachable: these pages render from the database, and
    a target that cannot be connected to is exactly what an operator sees before
    they run the checks. Where a state only appears when something is wrong — a
    latency-critical host, a build host that is the instance default — it is
    created deliberately, since those are the ones worth looking at.
    """
    ids: dict[str, str] = {}

    # ---- a key in the secret store ----
    csrf = session.csrf("/secrets")
    session.post(
        "/secrets",
        {
            "csrf": csrf,
            "name": "DEPLOY_SSH_KEY",
            "value": "-----BEGIN OPENSSH PRIVATE KEY-----\nnot-a-real-key\n-----END OPENSSH PRIVATE KEY-----\n",
            "scope": "",
        },
    )
    session.post(
        "/secrets",
        {
            "csrf": csrf,
            "name": "DATABASE_URL",
            "value": "postgres://user:pw@db.internal/app",
            "scope": "",
        },
    )
    secret_id = first_match(session.get("/build-hosts/new"), r'option value="(sec_[^"]+)"')
    ids["secret_id"] = secret_id or ""

    # ---- targets ----
    csrf = session.csrf("/targets/new")
    location = session.post(
        "/targets",
        {
            "csrf": csrf,
            "name": "edge-1",
            "host": "10.0.0.5",
            "port": "22",
            "user": "root",
            "ssh_key_id": secret_id or "",
            "labels": "env=prod\nrole=edge",
        },
    )
    ids["target_id"] = location.rsplit("/", 1)[-1]

    # Ingress on the ordinary target, so the populated shots show the enabled
    # card — routes, version, actions — rather than only the enable form. It
    # stays "pending" because there is no real host to install Caddy on, which
    # is itself a state worth being able to look at.
    session.post(
        f"/targets/{ids['target_id']}/ingress/enable",
        {
            "csrf": session.csrf(f"/targets/{ids['target_id']}"),
            "mode": "managed",
            "acme_email": "ops@example.com",
        },
    )

    session.post(
        "/targets",
        {
            "csrf": csrf,
            "name": "hft-box",
            "host": "10.0.0.7",
            "port": "22",
            "user": "root",
            "ssh_key_id": secret_id or "",
            "latency_critical": "1",
            "allow_latency_critical": "1",
            "labels": "env=prod\nrole=trading",
        },
    )

    # ---- build hosts ----
    csrf = session.csrf("/build-hosts/new")
    location = session.post(
        "/build-hosts",
        {
            "csrf": csrf,
            "name": "builder-1",
            "host": "10.0.0.9",
            "port": "22",
            "user": "build",
            "ssh_key_id": secret_id or "",
            "workspace_root": "/var/lib/nudo/builds",
            "labels": "arch=amd64\npool=ci",
        },
    )
    ids["build_host_id"] = location.rsplit("/", 1)[-1]

    location = session.post(
        "/build-hosts",
        {
            "csrf": csrf,
            "name": "gpu-box",
            "host": "10.0.0.42",
            "port": "22",
            "user": "build",
            "ssh_key_id": secret_id or "",
            "workspace_root": "/mnt/fast/builds",
            "latency_critical": "1",
            "allow_latency_critical": "1",
            "labels": "arch=arm64\naccel=cuda",
        },
    )
    ids["hot_build_host_id"] = location.rsplit("/", 1)[-1]

    # The instance default, so the list shows the badge and the detail page
    # shows the "services that do not name one build here" line.
    csrf = session.csrf("/build-hosts")
    session.post(
        "/build-hosts/default",
        {"csrf": csrf, "build_host_id": ids["build_host_id"]},
    )

    # ---- a service pinned to a build host ----
    csrf = session.csrf("/services/new")
    location = session.post(
        "/services",
        {
            "csrf": csrf,
            "name": "hft-bot",
            "target_id": ids["target_id"],
            "release_root": "/opt/hft-bot",
            "keep_releases": "5",
            "artifact_kind": "git",
            "git_repo": "acme/hft-bot",
            "git_branch": "main",
            "git_build_command": "cargo build --release",
            "git_artifact_path": "target/release/hft-bot",
            "git_build_host_id": ids["build_host_id"],
            "unit_name": "hft-bot.service",
            "description": "the trading bot",
            "restart": "always",
            "restart_sec": "2",
            "cpu_affinity": "2-5",
            "nice": "-10",
            "io_scheduling_class": "realtime",
            "health_kind": "http",
            "health_http_url": "http://127.0.0.1:9000/healthz",
            "health_timeout_seconds": "5",
            "health_retries": "6",
            "health_initial_delay_seconds": "2",
            "env": "RUST_LOG=info",
        },
    )
    ids["service_id"] = location.rsplit("/", 1)[-1]

    # ---- a routed service, so the ingress card has a route to show ----
    # Deliberately not the trading bot: that one carries the latency knobs, and
    # putting a proxy in front of it would be the wrong thing to illustrate.
    session.post(
        "/services",
        {
            "csrf": session.csrf("/services/new"),
            "name": "api",
            "target_id": ids["target_id"],
            "release_root": "/opt/api",
            "keep_releases": "5",
            "artifact_kind": "upload",
            "unit_name": "api.service",
            "description": "the public API",
            "restart": "always",
            "restart_sec": "5",
            "routes": "api.example.com:8080\napi-internal.example.com:8080",
            "health_kind": "http",
            "health_http_url": "http://127.0.0.1:8080/healthz",
            "health_timeout_seconds": "5",
            "health_retries": "3",
            "health_initial_delay_seconds": "2",
            "env": "",
        },
    )

    return ids


def first_match(haystack: str, pattern: str) -> str | None:
    match = re.search(pattern, haystack)
    return match.group(1) if match else None


# ---------------------------------------------------------------------------
# Driving it
# ---------------------------------------------------------------------------


def capture_all(
    chrome: Chrome,
    base: str,
    state: str,
    ids: dict[str, str],
    only: str | None,
    themes: list[str],
) -> int:
    taken = 0
    for name, template in VIEWS:
        if only and only not in name:
            continue
        if state == "empty" and name in NEEDS_DATA:
            continue
        try:
            path = template.format(**ids)
        except KeyError:
            # A view whose id was never created — say so rather than skipping
            # silently, since a missing id usually means seeding half-failed.
            print(f"  ! {name}: no id available, skipped")
            continue

        for theme in themes:
            chrome.set_theme(theme == "dark")
            directory = OUT / state / theme
            directory.mkdir(parents=True, exist_ok=True)
            target = directory / f"{name}.png"
            chrome.capture(base + path, target)
            taken += 1
            print(f"  {state}/{theme}/{name}.png")
    return taken


def run(args: list[str]) -> str:
    result = subprocess.run(args, capture_output=True, text=True)
    if result.returncode != 0:
        raise RuntimeError(f"{' '.join(args)} failed:\n{result.stdout}{result.stderr}")
    return result.stdout


def wait_for(what: str, seconds: int, probe):
    deadline = time.time() + seconds
    while time.time() < deadline:
        value = probe()
        if value:
            return value
        time.sleep(0.5)
    raise RuntimeError(f"timed out after {seconds}s waiting for {what}")


def find_chrome() -> str:
    for candidate in CHROME_CANDIDATES:
        if candidate and os.path.exists(candidate):
            return candidate
    raise SystemExit(
        "Google Chrome or Chromium is required to render the pages.\n"
        "Install it, or pass --chrome with the path to a binary."
    )


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Screenshot every dashboard view, empty and populated."
    )
    parser.add_argument(
        "--url",
        help="capture an instance already running instead of starting one; "
        "it must be signed in as the account this script creates, or fresh",
    )
    parser.add_argument("--image", default="nudo:dev", help="image to run (default nudo:dev)")
    parser.add_argument("--chrome", help="path to a Chrome or Chromium binary")
    parser.add_argument("--only", help="capture only views whose name contains this")
    parser.add_argument(
        "--state",
        choices=["empty", "populated", "both"],
        default="both",
        help="which state to capture (default both)",
    )
    parser.add_argument(
        "--theme",
        choices=["light", "dark", "both"],
        default="light",
        help="which colour scheme to capture (default light; the two differ only "
        "in palette, so capturing both doubles the images to review for little)",
    )
    parser.add_argument("--keep", action="store_true", help="leave the instance running")
    parser.add_argument("--width", type=int, default=1440)
    args = parser.parse_args()

    chrome_binary = args.chrome or find_chrome()
    themes = ["light", "dark"] if args.theme == "both" else [args.theme]
    states = ["empty", "populated"] if args.state == "both" else [args.state]

    instance = None
    if args.url:
        base = args.url.rstrip("/")
    else:
        if not shutil.which("docker"):
            raise SystemExit("Docker is required, or pass --url for a running instance.")
        instance = Instance(args.image)
        print(f"==> starting {args.image} on {WEB_PORT}")
        instance.start()
        base = instance.url

    # Only a full run clears the directory. A filtered or partial one overwrites
    # what it captures and leaves the rest alone: `--only build` wiping the other
    # thirty images is precisely the surprise that costs someone the run they
    # were comparing against.
    #
    # "Full" is measured against the defaults rather than against every possible
    # flag: a default run is what someone means by "regenerate the set", and it
    # should not leave a stale dark/ directory behind from an earlier `--theme
    # both`.
    full_run = not args.only and args.state == "both" and args.theme == parser.get_default("theme")
    if full_run and OUT.exists():
        shutil.rmtree(OUT)

    chrome = None
    taken = 0
    try:
        print("==> signing in")
        session = sign_in_at(base)

        chrome = Chrome(chrome_binary, args.width, 1400)
        chrome.set_cookie("nudo_session", session.session_id(), base)

        if "empty" in states:
            print("==> empty")
            taken += capture_all(chrome, base, "empty", {}, args.only, themes)

        if "populated" in states:
            print("==> seeding")
            ids = seed(session)
            print("==> populated")
            taken += capture_all(chrome, base, "populated", ids, args.only, themes)

    finally:
        if chrome:
            chrome.close()
        if instance and not args.keep:
            instance.stop()
        elif instance:
            print(f"==> left running at {instance.url} ({EMAIL} / {PASSWORD})")

    total = len(list(OUT.rglob("*.png"))) if OUT.exists() else 0
    if taken == total:
        print(f"\n{taken} screenshots in {OUT}")
    else:
        # Say both numbers, so a filtered run is not mistaken for a full one.
        print(f"\n{taken} captured, {total} total in {OUT}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

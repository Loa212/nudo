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

Two views cannot be reached by navigating to a URL on the demo instance, and
are captured against a second one started from the host binary in the release
layout — the shape `packaging/nudo.service` installs:

* **update-dialog-managed** — the dialog with a working "Update now", which a
  container never shows because a container cannot replace its own image.
* **update-dialog-in-progress** — what replaces the release notes once that
  button is pressed. Reached by pressing it for real against a fixture release,
  so the phases on screen are the ones the engine actually reports.

    scripts/screenshots.py                  # start a demo, capture, tear down
    scripts/screenshots.py --keep           # leave the instance running
    scripts/screenshots.py --only build     # views whose name contains "build"
    scripts/screenshots.py --only update    # just the update flow
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
    # The update flow. `#whats-new` opens the dialog, which is `:target`-driven
    # precisely so it can be linked to — which also makes it screenshottable
    # without driving clicks.
    ("update-dialog", "/#whats-new"),
    ("upgrade", "/upgrade"),
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
        # Two captures of the same URL in one run must not be the same bytes.
        # The update flow photographs `/` twice — once offering the upgrade,
        # once with it running — and a cached response makes the second shot a
        # copy of the first, which looks like a rendering bug rather than a
        # caching one.
        self.call("Network.setCacheDisabled", cacheDisabled=True)

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
        # Navigating to a URL that differs from the current one only by its
        # fragment moves the hash without reloading, so the second capture of
        # `/#whats-new` in a run would silently be a copy of the first — the
        # dialog before and during an upgrade are exactly that pair. A unique
        # query makes every navigation a real one.
        if "#" in url:
            base, fragment = url.split("#", 1)
            separator = "&" if "?" in base else "?"
            url = f"{base}{separator}_={secrets.token_hex(4)}#{fragment}"
        self.call("Page.navigate", url=url)
        # A fixed settle rather than waiting on a load event: the pages that
        # matter here stream (deployments, logs) and never reach quiescence.
        time.sleep(1.2)

        # A spinner is mid-rotation whenever the shutter happens to fall, so
        # an animated page photographs at a random angle — and a screenshot
        # taken twice differs for no reason anybody can act on. Holding every
        # animation at a quarter turn makes the frame deterministic and makes
        # the spinner legible as a spinner rather than as a broken ring.
        self.call(
            "Runtime.evaluate",
            expression="""
                document.getAnimations().forEach((animation) => {
                    animation.pause();
                    animation.currentTime = 175;
                });
            """,
            returnByValue=True,
        )

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

    def __init__(self, image: str, manifest_port: int | None = None) -> None:
        self.url = f"http://127.0.0.1:{WEB_PORT}"
        self.image = image
        self.manifest_port = manifest_port

    def start(self) -> None:
        subprocess.run(
            ["docker", "rm", "-f", CONTAINER],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        key = secrets.token_hex(32)
        args = [
            "docker", "run", "-d", "--name", CONTAINER,
            "-p", f"{WEB_PORT}:3000",
            "-e", f"NUDO_SECRET_KEY={key}",
            "-e", f"NUDO_BASE_URL={self.url}",
        ]
        if self.manifest_port:
            # The instance fetches the manifest from this host, so the update
            # banner and dialog are populated by the real release check rather
            # than by rows written behind its back.
            args += [
                "-e",
                "NUDO_UPDATE_MANIFEST_URL="
                f"http://host.docker.internal:{self.manifest_port}/releases.json",
                "--add-host",
                "host.docker.internal:host-gateway",
            ]
        args.append(self.image)
        run(args)
        wait_for("nudo to serve the dashboard", 120, self._serving)

    def _serving(self):
        try:
            with urllib.request.urlopen(f"{self.url}/login", timeout=2) as response:
                return response.status == 200 or None
        except (urllib.error.URLError, OSError):
            return None

    def wait_for_update_check(self, session: Session) -> bool:
        """Waits for the release check to have run at least once.

        It is deliberately staggered ~30s after boot so a restarted fleet does
        not fetch in lockstep, and there is no "check now" button — an operator
        never needs one. A screenshot run does, so it waits rather than
        reaching past the code that owns this.
        """
        def checked():
            return "9.9.0" in session.get("/") or None

        try:
            wait_for("the release check to find the pending release", 90, checked)
            return True
        except RuntimeError:
            print("  ! the release check did not run in time; the banner will be absent")
            return False

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


# The release the instance is told about, so the banner and its dialog have
# something to show. A version nothing will reach, so it stays "newer" whatever
# this build reports as its own.
PENDING_VERSION = "9.9.0"
PENDING_MANIFEST = {
    "releases": [
        {
            "version": PENDING_VERSION,
            "published_at": "2026-08-04",
            "url": f"https://github.com/Loa212/nudo/releases/tag/v{PENDING_VERSION}",
            "breaking": False,
            "notes": (
                "- Custom domains: managed ingress and automatic HTTPS for deployed services\n"
                "- Builds can run on a host other than the control plane\n"
                "- SSH host keys are verified and pinned on first use\n"
                "- Deploy artifacts are streamed and checked as they arrive, not buffered whole"
            ),
            # Filled in from the fixture tarball's real bytes before the
            # manifest is served: the engine verifies the download against
            # this, and a placeholder digest is correctly refused — which is
            # the check working, but makes for a dull screenshot.
            "artifacts": {},
        }
    ]
}


def manifest_with_digest(tarball: bytes) -> dict:
    """The pending-release manifest, carrying the fixture tarball's real digest.

    Not decoration: the upgrade verifies the download against exactly this
    value and refuses on a mismatch, so a manifest with a placeholder digest
    produces a screenshot of the refusal rather than of the progress view.
    """
    import copy
    import hashlib

    manifest = copy.deepcopy(PENDING_MANIFEST)
    name = f"nudo-v{PENDING_VERSION}-x86_64-unknown-linux-musl.tar.gz"
    manifest["releases"][0]["artifacts"] = {
        name: {"sha256": hashlib.sha256(tarball).hexdigest()}
    }
    return manifest


class ManifestServer:
    """Serves a release manifest to the instance under capture.

    The update banner and its dialog only render once the release check has
    found something newer, and a screenshot run cannot wait for a real release.
    Rather than writing the database rows directly — the image ships no sqlite3,
    and faking rows would capture a path no operator ever takes — this serves a
    manifest and lets the instance's own update check fetch it. What ends up on
    screen came through the real code.
    """

    def __init__(self, manifest: dict) -> None:
        self.body = json.dumps(manifest).encode()
        self.listener = socket.socket()
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind(("0.0.0.0", 0))
        self.listener.listen(16)
        self.port = self.listener.getsockname()[1]
        self.running = True
        self.thread = __import__("threading").Thread(target=self._serve, daemon=True)
        self.thread.start()

    def _serve(self) -> None:
        while self.running:
            try:
                conn, _ = self.listener.accept()
            except OSError:
                return
            try:
                conn.recv(4096)
                conn.sendall(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n"
                    b"content-length: " + str(len(self.body)).encode() + b"\r\n"
                    b"connection: close\r\n\r\n" + self.body
                )
            except OSError:
                pass
            finally:
                conn.close()

    def close(self) -> None:
        self.running = False
        try:
            self.listener.close()
        except OSError:
            pass


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


class ManagedInstance:
    """A nudo running the way a self-upgradable binary install actually runs.

    The demo instance is a container, and a container is correctly refused a
    self-upgrade — so it can never show "Update now" or the progress view. This
    starts a second one from the host binary in the release layout, with both
    opt-ins given: the same shape `packaging/nudo.service` installs.

    Its artifact server hands out a tarball that is deliberately not a working
    binary. The download and the digest check are real, and the upgrade stops
    at the exec — which is the failure the design is proudest of, and leaves
    this instance still serving the page being captured.
    """

    def __init__(self, port: int, manifest_port: int) -> None:
        self.url = f"http://127.0.0.1:{port}"
        self.port = port
        self.manifest_port = manifest_port
        self.root = pathlib.Path(tempfile.mkdtemp(prefix="nudo-managed-"))
        self.proc: subprocess.Popen | None = None
        self.artifacts: ArtifactServer | None = None

    def start(self) -> None:
        # Built with `self-upgrade-test`, which lifts one production check: the
        # requirement that the host be x86_64 Linux, since that is the only
        # target release artifacts are published for. Everything else about the
        # path — the layout, both opt-ins, the download, the digest check — is
        # the real thing, so the screenshots show real behaviour rather than a
        # mock. Without it, a developer machine is correctly told it cannot
        # self-upgrade and the button never appears.
        print("  building nudo-all-in-one with the self-upgrade test seam")
        run(
            [
                "cargo", "build", "--bin", "nudo-all-in-one",
                "--features", "nudo-allinone/self-upgrade-test",
            ]
        )
        binary = ROOT / "target" / "debug" / "nudo-all-in-one"
        if not binary.exists():
            raise RuntimeError(f"{binary} does not exist after the build")

        version = current_cargo_version()
        release = self.root / "self" / "releases" / version
        release.mkdir(parents=True)
        shutil.copy2(binary, release / "nudo-all-in-one")
        (self.root / "self" / "current").symlink_to(f"releases/{version}")

        # The "release" the upgrade downloads. Its digest is what the manifest
        # published, so verification passes; the payload is not a working
        # binary, so the upgrade stops at the exec — the failure the design is
        # proudest of, and the one that leaves this instance still serving.
        self.artifacts = ArtifactServer(FIXTURE_TARBALL)

        env = dict(os.environ)
        env.update(
            {
                "NUDO_SELF_DIR": str(self.root / "self"),
                "NUDO_SELF_UPGRADE_DOWNLOAD_BASE": f"http://127.0.0.1:{self.artifacts.port}",
                "NUDO_UPDATE_MANIFEST_URL": f"http://127.0.0.1:{self.manifest_port}/releases.json",
                "NUDO_DB": str(self.root / "nudo.db"),
                "NUDO_DATA_DIR": str(self.root / "data"),
                "NUDO_SECRET_KEY": secrets.token_hex(32),
                "NUDO_WEB_ADDR": f"127.0.0.1:{self.port}",
                "NUDO_GRPC_ADDR": f"127.0.0.1:{free_port()}",
                "NUDO_BASE_URL": self.url,
                "RUST_LOG": "warn",
            }
        )
        self.proc = subprocess.Popen(
            [str(self.root / "self" / "current" / "nudo-all-in-one")],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        wait_for("the managed instance to serve the dashboard", 60, self._serving)

    def _serving(self):
        try:
            with urllib.request.urlopen(f"{self.url}/login", timeout=2) as response:
                return response.status == 200 or None
        except (urllib.error.URLError, OSError):
            return None

    def enable_self_upgrade(self, session: Session) -> None:
        session.post(
            "/settings/self-upgrade",
            {"csrf": session.csrf("/settings"), "enabled": "on"},
        )

    def stop(self) -> None:
        if self.proc:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        if self.artifacts:
            self.artifacts.close()
        shutil.rmtree(self.root, ignore_errors=True)


class ArtifactServer:
    """Serves one tarball to whatever asks for it."""

    def __init__(self, tarball: bytes) -> None:
        self.body = tarball
        self.listener = socket.socket()
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(8)
        self.port = self.listener.getsockname()[1]
        self.running = True
        __import__("threading").Thread(target=self._serve, daemon=True).start()

    def _serve(self) -> None:
        while self.running:
            try:
                conn, _ = self.listener.accept()
            except OSError:
                return
            try:
                conn.recv(4096)
                # Slowly: the point of this server is to keep an upgrade in its
                # download phase long enough to photograph.
                conn.sendall(
                    b"HTTP/1.1 200 OK\r\ncontent-length: " + str(len(self.body)).encode()
                    + b"\r\nconnection: close\r\n\r\n"
                )
                # Deliberately slow. Over loopback the real sequence — download,
                # verify, stage, snapshot, swap — finishes in well under a
                # second, which is the right behaviour and an impossible thing
                # to photograph. Dripping the body holds the upgrade in its
                # download phase long enough for a screenshot without faking
                # any part of it.
                # ~15s to deliver the whole body, which comfortably outlasts a
                # page load plus Chrome's settle. The client buffers ahead, so
                # the pace has to be set here rather than by the body's size.
                chunk = max(len(self.body) // 60, 1024)
                for offset in range(0, len(self.body), chunk):
                    if not self.running:
                        break
                    try:
                        conn.sendall(self.body[offset : offset + chunk])
                    except OSError:
                        return  # the client gave up; nothing to report
                    time.sleep(0.25)
            except OSError:
                pass
            finally:
                conn.close()

    def close(self) -> None:
        self.running = False
        try:
            self.listener.close()
        except OSError:
            pass


def build_fixture_tarball() -> bytes:
    """A release archive with the right shape and a binary that cannot exec.

    Padded so the download takes long enough to be caught mid-flight.
    """
    import io
    import tarfile

    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:gz") as archive:
        # Incompressible, so the gzipped archive is genuinely large: zeros
        # would deflate to nothing and the drip-feed would have nothing to
        # drip. Large enough that the download outlasts a page capture.
        payload = b"#!/nonexistent\n" + secrets.token_bytes(24 * 1024 * 1024)
        info = tarfile.TarInfo(
            f"nudo-v{PENDING_VERSION}-x86_64-unknown-linux-musl/nudo-all-in-one"
        )
        info.size = len(payload)
        info.mode = 0o755
        archive.addfile(info, io.BytesIO(payload))
    return buffer.getvalue()


# Built once: the manifest must publish this exact tarball's digest, and the
# artifact server must serve exactly these bytes, or verification refuses it.
FIXTURE_TARBALL = build_fixture_tarball()


def current_cargo_version() -> str:
    text = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    return first_match(text, r'^version = "([^"]+)"') or "0.0.0"


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def capture_managed_update(chrome: Chrome, manifest_port: int, themes: list[str]) -> int:
    """Captures the two states only a self-upgradable install can show.

    `update-dialog-managed` is the dialog with a real "Update now" button;
    `update-dialog-in-progress` is what replaces the notes once it is pressed.
    """
    instance = ManagedInstance(WEB_PORT + 1, manifest_port)
    taken = 0
    try:
        instance.start()
        session = sign_in_at(instance.url)
        instance.enable_self_upgrade(session)
        # Chrome's cookie is scoped to an origin, and this instance is on a
        # different port from the demo — without its own cookie every capture
        # here is the login page.
        chrome.set_cookie("nudo_session", session.session_id(), instance.url)
        try:
            wait_for(
                "the managed instance's release check",
                90,
                lambda: (PENDING_VERSION in session.get("/")) or None,
            )
        except RuntimeError:
            print("  ! the managed instance never saw the release; skipping")
            return 0

        for theme in themes:
            chrome.set_theme(theme == "dark")
            directory = OUT / "populated" / theme
            directory.mkdir(parents=True, exist_ok=True)
            chrome.capture(instance.url + "/#whats-new", directory / "update-dialog-managed.png")
            taken += 1
            print(f"  populated/{theme}/update-dialog-managed.png")

        # Press the button for real, taking the token from inside the upgrade
        # form: the dashboard carries several, and the first one belongs to the
        # support banner.
        page = session.get("/")
        form = page.split('action="/upgrade/start"')[1][:600]
        csrf = first_match(form, r'name="csrf" value="([^"]*)"') or ""
        session.post("/upgrade/start", {"csrf": csrf, "target_version": PENDING_VERSION})

        # Each capture needs the upgrade to still be running when Chrome
        # renders, and one upgrade does not outlast several captures — so each
        # theme gets its own run. Restarting is cheap: the failed exec leaves
        # the instance serving, and the engine is happy to be asked again.
        for index, theme in enumerate(themes):
            if index > 0:
                page = session.get("/")
                if 'action="/upgrade/start"' not in page:
                    print("  ! the upgrade cannot be restarted; skipping the rest")
                    break
                form = page.split('action="/upgrade/start"')[1][:600]
                csrf = first_match(form, r'name="csrf" value="([^"]*)"') or ""
                session.post(
                    "/upgrade/start", {"csrf": csrf, "target_version": PENDING_VERSION}
                )

            try:
                wait_for(
                    "the upgrade to reach a phase worth showing",
                    30,
                    lambda: ("upgrade-step current" in session.get("/")) or None,
                )
            except RuntimeError:
                print("  ! the upgrade finished before it could be captured")

            chrome.set_theme(theme == "dark")
            directory = OUT / "populated" / theme
            chrome.capture(
                instance.url + "/#whats-new", directory / "update-dialog-in-progress.png"
            )
            taken += 1
            print(f"  populated/{theme}/update-dialog-in-progress.png")
    finally:
        instance.stop()
    return taken


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
    manifest_server = None
    if args.url:
        base = args.url.rstrip("/")
    else:
        if not shutil.which("docker"):
            raise SystemExit("Docker is required, or pass --url for a running instance.")
        # Served before the instance starts, so its first release check finds
        # it and the update banner has something to show.
        manifest_server = ManifestServer(manifest_with_digest(FIXTURE_TARBALL))
        instance = Instance(args.image, manifest_port=manifest_server.port)
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
            if instance:
                instance.wait_for_update_check(session)
            print("==> populated")
            taken += capture_all(chrome, base, "populated", ids, args.only, themes)

            # Last, and against a second instance: the demo runs in a
            # container, and a container is correctly refused a self-upgrade,
            # so the button and the progress view can only be shown by an
            # install shaped the way the packaged unit installs one.
            if manifest_server and (not args.only or "update" in args.only):
                print("==> the self-upgradable install")
                taken += capture_managed_update(chrome, manifest_server.port, themes)

    finally:
        if chrome:
            chrome.close()
        if manifest_server:
            manifest_server.close()
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

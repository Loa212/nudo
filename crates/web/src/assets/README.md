# Vendored front-end assets

Third-party JavaScript and CSS, committed here and embedded into the `nudo-web`
binary with `include_bytes!` (see `../assets.rs`), served from `/assets/`.

They are vendored rather than loaded from a CDN for three reasons: the binary is
self-contained and needs no network egress to render a page, there is no
JavaScript build step to run or keep working, and a control plane that manages
production hosts should not fetch executable code from a third party at page
load.

| File | Version | License | Purpose |
|---|---|---|---|
| `htmx.min.js` | 2.0.4 | 0BSD | Hypermedia interactions (`hx-get`, swaps) |
| `sse.js` | htmx 2.0.4 ext | 0BSD | Server-Sent Events (`sse-connect`, `sse-swap`) |
| `xterm.js` | 5.5.0 | MIT | Terminal emulator for the PTY view |
| `xterm.css` | 5.5.0 | MIT | Terminal styling |
| `xterm-addon-fit.js` | 0.10.0 | MIT | Sizes the terminal to its container |

`app.css` and `terminal.js` are part of this project, not vendored.

To update, re-fetch the pinned versions:

```sh
curl -fsSL https://unpkg.com/htmx.org@2.0.4/dist/htmx.min.js            -o htmx.min.js
curl -fsSL https://unpkg.com/htmx.org@2.0.4/dist/ext/sse.js             -o sse.js
curl -fsSL https://unpkg.com/@xterm/xterm@5.5.0/lib/xterm.js            -o xterm.js
curl -fsSL https://unpkg.com/@xterm/xterm@5.5.0/css/xterm.css           -o xterm.css
curl -fsSL https://unpkg.com/@xterm/addon-fit@0.10.0/lib/addon-fit.js   -o xterm-addon-fit.js
```

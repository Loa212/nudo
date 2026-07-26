# Examples

Three services that demonstrate what nudo does, each deployable to the demo
target with one command. They are shell scripts rather than compiled binaries so
the examples stay self-contained — what is being shown is the deploy mechanics,
not a build toolchain.

```sh
make demo                              # everything below, in one command
make example-deploy EXAMPLE=hello-http # or one at a time
```

## `hello-http`

A tiny HTTP service with an **HTTP health check**. The deploy is not finished
until the service actually answers on its port — so a release that starts but
never binds is a failed deploy, not a successful one.

## `latency-critical`

The reason this tool exists instead of Docker. Every latency knob is set —
`CPUAffinity=0-1`, `Nice=-10`, `IOSchedulingClass=realtime`, plus
`LimitMEMLOCK`, `LimitNOFILE` and `OOMScoreAdjust` as extra directives — and the
service logs what systemd *actually applied*, so the log viewer proves the
settings reached the host.

```sh
make example-unit EXAMPLE=latency-critical   # the unit file a deploy writes
make example-logs EXAMPLE=latency-critical   # what systemd gave it
make demo-units                              # cross-check on the target
```

## `flaky`

Starts cleanly and never becomes ready. systemd reports the unit **active**,
because the process is running and has not exited — only a health check notices
that it is not serving. This is the case a tool that just runs `systemctl
restart` gets wrong.

```sh
make example-deploy EXAMPLE=flaky   # a healthy release first
make example-break                  # then a broken one, and watch the rollback
```

## Writing your own

Add a directory under `services/` with two files:

- **`run.sh`** — anything executable. nudo ships it as the release binary and
  points the unit's `ExecStart` at it.
- **`service.json`** — the service definition. The field names match the
  dashboard's form, so anything here can also be typed into the UI. `$comment`
  is documentation and is dropped before submission.

`make help` then lists it automatically.

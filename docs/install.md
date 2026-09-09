# Installing dreamd as a per-user service

`dreamd service install` registers `dreamd watch` with your login session's
service manager — a systemd `--user` unit on Linux, a LaunchAgent on macOS — so
the daemon starts at login and is restarted if it dies. `dreamd service start`
starts it on demand; `dreamd service restart` bounces it; `dreamd service
status` reports what the service manager thinks of it. All four verbs shell out
to `systemctl --user` / `launchctl` as you; none needs `sudo`, and none should
ever be run with it.

This is **optional**. If you run one agent, the in-process MCP server that
`npx -y dreamd-mcp` starts is enough — nothing on this page is required.
Install the service when several agents on one machine share a project and you
want one long-lived `dreamd watch` to be the single writer, instead of leaving
it running in a terminal ([GUIDE.md §5](../GUIDE.md#5-daemon-mode)).

## One daemon per user, not per machine

The daemon binds `~/.agent/dreamd.sock` with mode `0600`, so only your user can
connect. The first process to bind it is the writer: a second `dreamd watch`
finds the live socket and exits instead of taking it over, while a stale socket
file left behind by a crashed daemon is unlinked and rebound. Every request is
then checked against the connecting peer's UID (`SO_PEERCRED` on Linux,
`getpeereid` on macOS) and rejected with `403` if it is not the daemon owner's
— see [../SECURITY.md](../SECURITY.md).

That is why the service is a **user** unit / LaunchAgent and not a system
daemon: it has to run as the same user as the agents that talk to it, and its
socket and log live under that user's home. Never `sudo dreamd service install`.
If you need the daemon under a different account, log in as that account and
run `dreamd watch` there.

## Linux (systemd `--user`)

Run from inside the project you want the daemon to serve:

```bash
cd ~/your-project
dreamd service install
```

This writes `~/.config/systemd/user/dreamd.service` (`Type=simple`,
`ExecStart=<this binary> watch`, `WorkingDirectory=<project root>`,
`Restart=on-failure`), then runs `systemctl --user daemon-reload` and
`systemctl --user enable --now dreamd.service`. Re-running it overwrites the
unit silently; `--force` is accepted and ignored on Linux.

To start it later without reinstalling:

```bash
dreamd service start          # systemctl --user start dreamd.service
```

To bounce a running one — after upgrading the binary in place, for example:

```bash
dreamd service restart        # systemctl --user restart dreamd.service
```

To check on it:

```bash
dreamd service status         # running / stopped / failed / not-installed, PID, last 10 log lines
journalctl --user -u dreamd.service   # optional: the daemon's stderr in the user journal
ls -l ~/.agent/dreamd.sock    # srw------- owned by you
tail -f ~/.agent/dreamd.log
```

`dreamd status` is the daemon-level view — Unix-socket liveness, the project
under your current directory, when the last dream cycle ran — and works for a
foreground `dreamd watch` too, whereas `dreamd service status` reports what
systemd (or launchd) thinks of the unit; the two can disagree, e.g. a foreground
`watch` live with no unit installed.

Without a systemd user instance (probe: `/run/systemd/system` — most
containers, WSL without systemd, non-systemd distros) all four verbs exit 2
and tell you to run `dreamd watch` in the foreground instead. Nothing is
written.

## macOS (LaunchAgent)

```bash
cd ~/your-project
dreamd service install
```

This writes `~/Library/LaunchAgents/dev.dreamd.dreamd.plist` — Label
`dev.dreamd.dreamd`, `ProgramArguments` = this binary + `watch`,
`WorkingDirectory` = the project root, `RunAtLoad`, and `KeepAlive` with
`SuccessfulExit=false` (restart on failure, stay down after a clean exit) — and
loads it with `launchctl bootstrap gui/$(id -u) <plist>`.

If the plist already exists, `install` refuses with exit 2 and leaves the
existing file untouched. Pass `--force` to overwrite it:

```bash
dreamd service install --force
```

`--force` **is** the confirmation — there is no interactive prompt, so the
command behaves the same from a script, a non-tty, or `npx`. It rewrites the
plist, runs `launchctl bootout gui/$(id -u)/dev.dreamd.dreamd` (errors ignored
— the agent may not be loaded yet), then `bootstrap`s the new file.

To (re)start the loaded agent:

```bash
dreamd service start          # launchctl kickstart -k gui/$(id -u)/dev.dreamd.dreamd
```

To bounce it:

```bash
dreamd service restart        # launchctl kickstart -k gui/$(id -u)/dev.dreamd.dreamd
```

On macOS that is the **same argv** as `start`: `kickstart -k` already kills the
running instance before relaunching it, so start's command *is* the bounce.
Only the line printed differs (`restarted …`).

To inspect it:

```bash
dreamd service status         # running / stopped / not-installed, PID, last 10 log lines
launchctl print gui/$(id -u)/dev.dreamd.dreamd   # raw launchd view of the agent
tail -f ~/.agent/dreamd.log
ls -l ~/.agent/dreamd.sock
```

Logs are in `~/.agent/dreamd.log`, written by `dreamd watch` itself through its
tracing file layer, not by launchd. The plist deliberately sets no
`StandardOutPath` / `StandardErrorPath` (that would be a second writer on a
file the daemon truncates at start) and no `EnvironmentVariables`.

The `gui/<uid>` domain requires a GUI login session. Over headless SSH — no
desktop session for your user — `bootstrap` and `kickstart` fail; run
`dreamd watch` in the foreground there instead.

## Restart is a bounce, not a reinstall

`dreamd service restart` restarts the **supervised process** and nothing else.
It never rewrites the unit or the plist, so `ExecStart` / `ProgramArguments`
stays the binary path captured at `dreamd service install`. That is exactly
what you want after upgrading a binary **in place** — `cargo install --path
crates/dreamd-cli` over the same path, say: the path is unchanged, so a bounce
picks up the new build. If the binary **moved** — an npx cache bump, a
different install prefix — restart would just relaunch the old path, so rerun
`dreamd service install` (`--force` on macOS) first.

`dreamd update --restart` is a different verb. Every `dreamd update` stops local
`mcp` / `watch` processes — `--restart` only says so out loud — and that includes
the one the service supervises, which runs under your `HOME`. Nothing is brought
back automatically: reload your MCP harness for the servers it spawns, and run
`dreamd service restart` for the service.

If no unit / LaunchAgent was ever installed, `restart` fails exactly the way
`start` does: the supervisor's own error, exit 1. A host with no supervisor at
all still exits 2 and points you at foreground `dreamd watch`.

## Which project the service serves

The unit's `WorkingDirectory` (the plist's `WorkingDirectory`) is the project
root discovered from your current directory at install time: the nearest
ancestor of that directory that contains a `.agent/` directory — that is, a
project where `dreamd init` has already been run. Without one, `install` exits
2 with:

```text
no .agent/ directory found. Run `dreamd init` first.
```

`dreamd watch` needs the project root too: with none it exits 2, and under a
supervisor that becomes a restart loop. Always run `install` from inside the
project.

There is one service per user, so it points at one project at a time. To move
it, `cd` into the other project and reinstall — silently on Linux, with
`--force` on macOS. The daemon still answers requests for other project roots
(every request names its root in `X-Agent-Root`, see
[http-api.md](./http-api.md)); the working directory decides which project the
daemon boots in and pins at start.

## Via npx

```bash
npx -y dreamd-mcp service install
npx -y dreamd-mcp service start
npx -y dreamd-mcp service restart
```

The shim forwards `service` to the native binary. The `ExecStart` /
`ProgramArguments` path written into the unit is that binary's own resolved
path (`current_exe`, canonicalized) — under `npx` that is the cached native
`dreamd` the shim downloaded, not `npx` or `node`. If you clear the npm cache
or upgrade the package, reinstall the service so the unit points at the new
binary (`--force` on macOS).

## Fallback: foreground `dreamd watch`

The service is a convenience around the same foreground process:

```bash
cd ~/your-project
dreamd watch                  # or: npx -y dreamd-mcp watch
```

Use this when there is no systemd user instance, on a headless macOS session,
on Windows (native Windows is out of scope — see [windows.md](./windows.md)), or
whenever you would rather see the daemon in a terminal. It binds the same
socket, writes the same log, and is exactly what the service supervises.

## Removing the service

```bash
dreamd service uninstall
```

On Linux that is `systemctl --user disable --now dreamd.service`, removing
`~/.config/systemd/user/dreamd.service`, then `systemctl --user daemon-reload`.
On macOS it is `launchctl bootout gui/$(id -u)/dev.dreamd.dreamd` and removing
`~/Library/LaunchAgents/dev.dreamd.dreamd.plist`. It is idempotent — a service
that is already gone is not an error — and it prints what it removed and what
it kept:

```text
removed: /home/you/.config/systemd/user/dreamd.service
preserved: /home/you/.agent (daemon home)
preserved: per-project .agent/ stores
```

**By default nothing is deleted but the service entry itself.** Your memory is
untouched: every project's `.agent/` store, and the daemon home `~/.agent/`
with its registry, socket and `dreamd.log`.

To also delete the daemon home, add `--purge`:

```bash
dreamd service uninstall --purge --yes
```

`--purge` removes `~/.agent/` — the registry, the socket and `dreamd.log` — and
nothing else. It **never** touches a per-project `<repo>/.agent/` store; to
clear one of those, see the *Full fresh store* row in
[troubleshooting.md](./troubleshooting.md#how-do-i-reset-or-clear-memory).
Because it is destructive it asks first, like `dreamd reset workspace`: pass
`--yes`, or answer `y` at the prompt. Without `--yes` on a non-interactive
stdin it refuses and changes nothing.

Note this is not `dreamd uninstall`, which is a different verb: that one stops
running `dreamd` processes, drops the socket and clears the download cache,
while leaving the systemd unit or LaunchAgent in place.

If you ever need to undo the service by hand — a unit installed by a build of
`dreamd` you no longer have, say — the equivalent commands are:

```bash
# Linux
systemctl --user disable --now dreamd.service
rm ~/.config/systemd/user/dreamd.service
systemctl --user daemon-reload

# macOS
launchctl bootout gui/$(id -u)/dev.dreamd.dreamd
rm ~/Library/LaunchAgents/dev.dreamd.dreamd.plist
```

## See also

- [../GUIDE.md §5](../GUIDE.md#5-daemon-mode) — running `dreamd watch` by hand
- [troubleshooting.md — Socket permission denied](./troubleshooting.md#socket-permission-denied)
- [../SECURITY.md](../SECURITY.md) — threat model and socket auth
- [../ARCHITECTURE.md §8.1](../ARCHITECTURE.md#81-vestigial--deferred-machinery-v01) — why the service supervises a foreground process and `detach_double_fork` stays unused

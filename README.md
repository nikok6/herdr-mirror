# herdr-mirror

A [herdr](https://herdr.dev) plugin that mirrors a remote herdr server's
workspaces and agents into your local sidebar. One window shows the agents on
every machine — blocked, working, done — with live pane content you can watch
and drive over ssh.

Each remote workspace becomes a real local workspace named `<host>: <name>`.
Its panes stream the remote terminal live; its agents report their real state.
Mirroring is one-way (the remote needs no plugin — just herdr), but you can
type into any mirror pane to drive the remote session, and create remote
workspaces/tabs/panes from your side.

> **How it works.** One Rust binary (`herdr-mirror`) with subcommand modes: a
> `daemon` (control plane — reconciles remote workspaces into local mirrors
> and pushes agent status) and one `pane` process per mirror pane (data plane
> — streams the remote terminal over ssh).

## Requirements

- **Both machines**: herdr with the `terminal session` streams — preview build
  `2026-06-30` or newer (`herdr channel set preview`), until the next stable.
- **Local machine**: macOS or Linux on x86_64/aarch64 — install fetches the
  prebuilt binary from Releases (dev installs via `herdr plugin link` build
  from source with `cargo build --release`).
- **ssh**: non-interactive key auth to each remote
  (`ssh -o BatchMode=yes <host> true` must succeed without a prompt).

## Installation

```bash
herdr plugin install <owner>/herdr-mirror     # or: herdr plugin link <path>
```

Then create the config (see location with `herdr plugin config-dir mirror`;
in dev it's `~/.config/herdr-mirror/hosts.toml`):

```toml
[hosts.work]
target = "work"        # anything ssh accepts: alias, user@host, ssh://host:2222
```

Start it — or just focus any workspace, which auto-starts the daemon:

```bash
./target/release/herdr-mirror start     # or the "Mirror: start" action
```

Within a few seconds `work: *` workspaces appear in your sidebar. Check state
any time with `herdr-mirror status`.

## Usage

**Watch** — mirror panes are read-only by default: a live view of the remote
pane with zero effect on the remote.

**Drive** — type into a mirror pane to take control (your keystroke is
delivered). Both sides now share the session, tmux-style. Control auto-releases
after 60s idle; `ctrl+\` releases immediately. The mouse wheel scrolls remote
scrollback while driving.

**Close / restore** — close a mirror like any pane (`prefix+x`); it won't come
back on its own. Run the **restore** action (`herdr-mirror restore`) to bring
back everything you've closed.

**Pause** — the **pause** action halts syncing; mirrors stay frozen in place
and resume with **start**. `teardown` closes all mirrors and clears state.

**Create on the remote** — four actions create objects on the remote host,
inheriting the target host and cwd from the mirror you invoke them from (the
same rule as native `prefix+shift+n`, across ssh): `remote-new-workspace`,
`remote-new-tab`, `remote-split-right`, `remote-split-down`. The new object
mirrors back within seconds.

**Continuous streaming** — every mirror pane streams its remote pane live for
its whole lifetime, each over its own direct ssh connection, so panes are never
blank and a busy pane can't contend with or drop another's stream. Sidebar
agent status is daemon-driven, not stream-derived, so every agent's state stays
live regardless of what any stream is doing.

### Keybinds

Actions have no default keys; bind them in `~/.config/herdr/config.toml`, then
`herdr server reload-config`:

```toml
[[keys.command]]
key = "prefix+shift+m"
type = "plugin_action"
command = "mirror.start"

[[keys.command]]
key = "prefix+shift+s"
type = "plugin_action"
command = "mirror.pause"
```

## Configuration

`hosts.toml`:

```toml
# autostart = true       # focusing a workspace starts the daemon (default).
                         # A manual pause is sticky until you start again;
                         # a crash still auto-recovers on next focus.
# poll_seconds = 60      # reconcile poll interval (events drive most syncs)
# default_host = "work"  # host that "new remote workspace" targets when
                         # invoked outside any mirror (default: first host)

[hosts.work]
target = "work"
# prefix = "work"                    # sidebar prefix (default: the host key)
# remote_bin = "~/.local/bin/herdr"  # remote path if it's not on ssh's PATH
# enabled = true                     # false stops syncing this host without
                                     # deleting its config; mirrors stay put

[hosts.vps]                          # add more hosts freely; each is independent
target = "ssh://niko@203.0.113.7:2222"
```

## Limitations

- **Version-locked to preview** until the `terminal session` streams reach
  stable; keep both machines on the same build.
- **Latency** above raw ssh: keystroke echo is a rendered frame round-trip, so
  there's a small constant delay. For latency-critical work, plain `ssh <host>`
  is always one command away.
- **Layout geometry is copied, not linked**: pane content and typing are
  always live — only sizes are a snapshot. Remote pane adds/removes reconcile,
  but a split-ratio change at the remote doesn't resize an existing mirror.
- **cwd-derived columns show local data** — git branch etc. reflect the machine
  ssh runs from, not the remote repo.
- **No custom sidebar UI** (plugin API limitation): mirrors are grouped only by
  the `<host>: ` naming convention.
- **Remote must be reachable and running herdr**; the daemon surfaces a
  readable status if a host is down or on too old a version.

## License

MIT — see [LICENSE](./LICENSE).

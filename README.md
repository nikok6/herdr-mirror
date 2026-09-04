# herdr-mirror

A [herdr](https://herdr.dev) plugin that mirrors a remote herdr server's
workspaces and agents into your local sidebar. One window shows the agents on
every machine — blocked, working, done — with live pane content you can watch
and drive.

<p align="center">
  <img src="assets/hero.png" width="720" alt="herdr-mirror: local and remote herdr sessions unified in one window">
</p>

> 🐾 Check out my other plugin: [herdr-pet](https://github.com/nikok6/herdr-pet), a tiny
> desk pet on your panes that types, waits, and celebrates with your agent.

Each remote workspace becomes a real local workspace named `<host>: <name>`.
Its panes stream the remote terminal live; its agents report their real state.
Mirroring is one-way (the remote needs no plugin — just herdr), but you can
type into any mirror pane to drive the remote session, and create remote
workspaces/tabs/panes from your side.

A remote can be another machine over **ssh**, or a **container** on this one
(see [Devcontainer](#devcontainer)) — same mirrors either way.

> **How it works.** One Rust binary (`herdr-mirror`) with subcommand modes: a
> `daemon` (control plane — reconciles remote workspaces into local mirrors
> and pushes agent status) and one `pane` process per mirror pane (data plane
> — streams the remote terminal).

## Requirements

- **Both ends**: herdr with the `terminal session` streams — preview build
  `2026-06-30` or newer (`herdr channel set preview`), until the next stable.
- **Local machine**: macOS or Linux on x86_64/aarch64 — install fetches the
  prebuilt binary from Releases (dev installs via `herdr plugin link` build
  from source with `cargo build --release`).
- **ssh hosts**: non-interactive key auth to each remote
  (`ssh -o BatchMode=yes <host> true` must succeed without a prompt).
- **Container hosts**: a working `docker` CLI, plus `socat` and herdr inside
  the image. No sshd, no keys, no published ports.
- **Image paste on Linux** (optional): `wl-paste` or `xclip`, to read the
  clipboard. macOS needs nothing; dropping files works either way.

## Installation

```bash
herdr plugin install nikok6/herdr-mirror     # or: herdr plugin link <path>
herdr server reload-config                   # load the plugin (actions + autostart hook)
```

Installing also links the CLI at `~/.local/bin/herdr-mirror`, so `herdr-mirror
<cmd>` works from a shell and keybindings get one stable path that survives
updates.

Then create the config at `~/.config/herdr-mirror/hosts.toml`:

```toml
[hosts.work]
target = "work"         # anything ssh accepts: alias, user@host, ssh://host:2222
# session = "project"   # mirror `herdr --session project` on that host
```

That's it — the daemon autostarts when you focus a workspace, so within a few
seconds `work: *` workspaces appear in your sidebar. Check state any time with
`herdr-mirror status`, which prints the config file it loaded.

> **Config location.** `~/.config/herdr-mirror/hosts.toml` is the canonical
> path: it's the only one reachable from *both* plugin actions and a plain
> shell. The plugin config dir (`herdr plugin config-dir mirror`) is also
> searched and takes precedence if a `hosts.toml` lives there, but prefer the
> canonical path unless you have a reason not to. If both exist, the ignored
> one is reported rather than silently dropped.

If you've disabled autostart, start the daemon yourself by keybinding the
"Mirror: start" action or running:

```bash
./target/release/herdr-mirror start
```

## Usage

**Drive** — the default (`always_control = true`) is tuned for headless remotes
(a vps, a server) with no window of their own: it keeps each mirror pane
writable and sized to your local pane, so the headless remote fills it instead
of showing a tiny default-sized window. Type and your keystrokes go to the
remote, tmux-style; the mouse wheel scrolls remote scrollback.

**Watch-only** — for a machine with its own display or a human sitting at it,
set `always_control = false` (globally or per host). Its mirrors become
read-only: a live view with zero effect on the remote that escalates to control
when you type and auto-releases after 1h idle (`ctrl+\` releases immediately).

**Close / restore** — by default, closing a mirror (`prefix+x`) also closes the
pane/tab/workspace on the remote (`close_remote_on_local_close`; set it false to
only stop mirroring and leave the remote — and its agent — running). When the
remote is left running, the **restore** action (`herdr-mirror restore`) brings
back mirrors you closed.

**Pause** — the **pause** action halts syncing; mirrors stay frozen in place
and resume with **start**. `teardown` closes all mirrors and clears state.

**Hide / show** — **hide** takes a connection's mirrors off the sidebar and
leaves the remote untouched: its workspaces, panes and agents keep running.
**show** brings them back. Both act on one host: name it (`herdr-mirror hide
work`), or invoke them inside a mirror to take that mirror's host.

**Create on the remote** — four actions create objects on the remote host,
inheriting the target host and cwd from the mirror you invoke them from (the
same rule as native `prefix+shift+n`, but remote): `remote-new-workspace`,
`remote-new-tab`, `remote-split-right`, `remote-split-down`. The new object
mirrors back within seconds.

**Native keys inside a mirror** — creating a tab or a split with herdr's own
keys while focused on a mirror pane creates it on the remote, instead of
dropping a local terminal into the mirrored workspace. The local object closes
as it appears, the mirror pane says so in its status row, and the remote one
mirrors back seconds later. A native new workspace opens the host picker.
Nothing outside a mirror workspace is affected.

**Pick a host** — `new-workspace-pick` opens that picker on demand: this
machine, or any configured host.

**One key for both worlds** — outside a mirror they degrade to the plain local
action instead of erroring, so one binding can replace the native key entirely.
Exception: on a non-mirrored pane inside a mirror workspace, `remote-split-*`
errors rather than splitting locally, which would desync the mirrored layout.

**Invoke any remote plugin** — locally bound keys never reach a mirror pane's
stdin, so remote plugins can't be driven by their own bindings. `remote-invoke
<plugin>.<action>` runs the action on the mirrored host behind your focused
pane, handing it the remote workspace, pane, and cwd; outside a mirror it runs
the action locally, so one key covers both worlds. The plugin must be
installed on whichever end runs it. See
[Remote plugin keys](#remote-plugin-keys) for binding it.

**Continuous streaming** — every mirror pane streams its remote pane live for
its whole lifetime, each over its own connection, so panes are never
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
command = "mirror.start"       # start / resume the daemon

[[keys.command]]
key = "prefix+shift+s"
type = "plugin_action"
command = "mirror.pause"       # freeze syncing; start again to resume

[[keys.command]]
key = "prefix+shift+b"         # "bring back" (shift+r is native reload_config)
type = "plugin_action"
command = "mirror.restore"     # un-close mirrors you closed locally

[[keys.command]]
key = "prefix+alt+d"           # destructive: closes ALL mirrors + clears state
type = "plugin_action"
command = "mirror.teardown"    # stop mirroring everything (start to resume)

[[keys.command]]
key = "prefix+alt+h"
type = "plugin_action"
command = "mirror.hide"        # take a host's mirrors off the sidebar

[[keys.command]]
key = "prefix+alt+shift+h"
type = "plugin_action"
command = "mirror.show"        # and put them back

[[keys.command]]
key = "prefix+shift+n"         # native new_workspace, now with a host to pick
type = "plugin_action"
command = "mirror.new-workspace-pick"

# Create objects on the REMOTE host, or locally when invoked outside a mirror.
# Each is herdr's native local key + alt (Option): same muscle memory, remote
# target.
[[keys.command]]
key = "prefix+alt+n"           # native new_workspace = prefix+shift+n
type = "plugin_action"
command = "mirror.remote-new-workspace"

[[keys.command]]
key = "prefix+alt+c"           # native new_tab = prefix+c
type = "plugin_action"
command = "mirror.remote-new-tab"

[[keys.command]]
key = "prefix+alt+v"           # native split_vertical = prefix+v
type = "plugin_action"
command = "mirror.remote-split-right"

[[keys.command]]
key = "prefix+alt+minus"       # native split_horizontal = prefix+minus
type = "plugin_action"
command = "mirror.remote-split-down"
```

Outside a mirror, `remote-new-workspace` targets `default_host` and the others
act locally. The remaining actions — `mirror.status`, `mirror.once`,
`mirror.ensure` — are lifecycle/diagnostic and are usually run from the CLI
rather than bound.

**If you live in mirrors, drop the alt variants.** Since these fall back to the
native behaviour anyway, hand them the native keys and keep one set of muscle
memory — remote inside a mirror, local everywhere else:

```toml
[keys]                             # move the natives out of the way
new_tab = "prefix+shift+c"
split_vertical = "prefix+shift+v"
split_horizontal = "prefix+shift+minus"

[[keys.command]]
key = "prefix+c"                   # was native new_tab
type = "plugin_action"
command = "mirror.remote-new-tab"

[[keys.command]]
key = "prefix+v"                   # was native split_vertical
type = "plugin_action"
command = "mirror.remote-split-right"

[[keys.command]]
key = "prefix+minus"               # was native split_horizontal
type = "plugin_action"
command = "mirror.remote-split-down"
```

The tradeoff: those keys now depend on this plugin being installed and enabled,
since the native binding no longer covers them.

### Remote plugin keys

`remote-invoke` takes the action to forward as an argument, which plugin
actions can't carry, so bind it as a `shell` command. Use the absolute
`~/.local/bin/herdr-mirror` path the install linked: herdr runs shell bindings
through a login `sh` that never reads `~/.zshrc`, so a bare `herdr-mirror`
works or silently fails depending on how herdr was launched, while the
absolute path works everywhere.

```toml
[[keys.command]]
key = "prefix+alt+l"
type = "shell"
command = "~/.local/bin/herdr-mirror remote-invoke lazygit.open"
```

One binding covers every machine: the action fires on the focused mirror's
host, or locally outside a mirror. When it can't fire (plugin not installed
there, typo, unreachable host, non-mirrored pane), a toast tells you why —
key-bound output is discarded, so the toast is the feedback channel.

Or skip the hand-editing entirely:

```bash
herdr-mirror remote-actions                  # list what each host + local can invoke
herdr-mirror bind lazygit.open prefix+alt+l  # write the block above + reload herdr
herdr-mirror unbind lazygit.open             # remove it again (or: unbind <key>)
```

`bind` writes the marked block to `~/.config/herdr/config.toml`, refuses keys
the file already binds, and reloads herdr, so the key is live immediately;
`unbind` removes only blocks that `bind` wrote. `remote-actions` also prints a
paste-ready binding block.

### Paste and drop files

**Ctrl+v** an image, or **drag files** onto a mirror pane. A local path means
nothing on the remote, so the files are uploaded to
`~/.cache/herdr-mirror/pastes/` there and the remote path is pasted instead,
ready for an agent to open.

Any file type, several at once, up to 32MB each. Ordinary pastes pass through
untouched, and paths that already exist on the remote are left alone. Reading
an image off the clipboard needs `wl-paste` or `xclip` on Linux (macOS and
dropped files need nothing). Uploads aren't cleaned up; `rm -rf
~/.cache/herdr-mirror/pastes` is always safe.

### Mouse

- **Drag to select, release to copy** — The text reaches your own clipboard, even over ssh.
- **Everything else goes to the program on the remote** — the wheel, right and
  middle click, and a plain click that isn't a drag.

## Configuration

`hosts.toml`:

```toml
# autostart = true       # focusing a workspace starts the daemon (default).
                         # A manual pause is sticky until you start again;
                         # a crash still auto-recovers on next focus.
# poll_seconds = 60      # reconcile poll interval (events drive most syncs)
# default_host = "work"  # host that "new remote workspace" targets when
                         # invoked outside any mirror (default: first host)
# close_remote_on_local_close = true
                         # default. Closing a mirror pane/tab/workspace
                         # locally (e.g. prefix+x) also closes it on the
                         # remote. Set false to only stop mirroring on a local
                         # close, leaving the remote pane and its agent running.
# always_control = true  # default. Mirror panes stay in control: writable, no
                         # idle release, and sized to your local pane so the
                         # remote fills it (ideal for headless remotes). Set
                         # false for read-only mirrors that escalate on type.
# max_cols / max_rows    # cap the size control asks the remote for, so a
                         # machine with its own display keeps its geometry.
                         # A ceiling only, and never applies to watch-only.

[hosts.work]
target = "work"
# prefix = "work"                    # sidebar prefix (default: the host key)
# remote_bin = "~/.local/bin/herdr"  # remote path if it's not on the remote PATH
# session = "project"                # mirror a named herdr session on this host
                                     # (`herdr --session project`)
# max_cols = 212                     # per-host size cap; pairs with
# max_rows = 58                      # always_control = false
# always_control = false             # per-host override, e.g. a host you use
                                     # directly (don't drive its pane sizes)
# enabled = true                     # false stops syncing this host without
                                     # deleting its config; mirrors stay put
# api_transport = "auto"             # how to reach the remote API socket:
                                     # "socket" = ssh -L forward, "exec" = relay
                                     # over ssh exec (needs socat or python3).
                                     # auto = socket, falling back to exec.
# git_status = true                  # default. Report each mirror workspace's
                                     # remote git state as $mgit_* tokens (see
                                     # "Git status on mirror rows" below).
# git_status_secs = 20               # probe cadence; minimum 5

[hosts.vps]                          # add more hosts freely; each is independent
target = "ssh://niko@203.0.113.7:2222"
```

### Git status on local rows too

The same tokens cover this machine's **native** workspaces (which otherwise
only get the built-in branch/ahead-behind chip — no clean/dirty/conflict). A
second relay runs in the daemon over plain `sh -c` — no ssh — probing each
local workspace's cwd on the same cadence and reporting under its own source
ID (`plugin:mirror:local:git`). Mirror workspaces are skipped: their tokens
are the per-host relays' job, and two reporters on one token key would fight.

| hosts.toml | effect |
|---|---|
| `git_status_local = false` | turn the local relay off (default: follows `git_status`) |
| `git_status_local_secs` | its cadence (default: follows `git_status_secs`); min 5 |
| `git_status_local_scope = "all"` | also cover mirror workspaces — only for the single-writer setup where `git_status = false` turned the per-host relays off |

### Git status on mirror rows

herdr derives the sidebar's branch/ahead-behind from the workspace's *local*
cwd, and mirror workspaces deliberately sit on a non-git cwd — so by default
they show no git chip. The daemon bridges that itself: every `git_status_secs`
it probes each mirror workspace's remote cwd with one batched
`git --no-optional-locks status --porcelain=v1 -b` (one ssh exec per host,
regardless of workspace count), and reports the result to the local mirror row
as `$mgit_*` metadata tokens with a TTL of three intervals — a host that dies
self-clears instead of lying forever.

| token          | content                                                |
|----------------|--------------------------------------------------------|
| `$mgit_branch` | the remote branch (elided on a detached HEAD)           |
| `$mgit_ab`     | `↑N` ahead / `↓N` behind upstream                       |
| `$mgit_clean`  | `✓` — nothing to commit, push, or pull                  |
| `$mgit_dirty`  | `+staged ~modified ?untracked` (non-zero parts only)    |
| `$mgit_conflict` | `!N` unmerged paths (beats dirty and clean)           |

herdr renders token values as flat text, so severity colour comes from naming
the tokens in `[ui.sidebar.spaces]` with an inline `fg` each — only the active
one of clean/dirty/conflict ever renders:

```toml
[ui.sidebar.spaces]
rows = [
  ["state_icon", "workspace"],
  ["branch", "git_status",
    { token = "$mgit_branch",   fg = "#a89984" },
    { token = "$mgit_ab",       fg = "#83a598" },
    { token = "$mgit_clean",    fg = "#b8bb26" },
    { token = "$mgit_dirty",    fg = "#fabd2f" },
    { token = "$mgit_conflict", fg = "#fb4934" }],
]
```

Then `herdr server reload-config`. On native (non-mirror) workspaces none of
the `$mgit_*` tokens are ever reported, so the same rows render them as
nothing and the built-in `branch`/`git_status` keep working as usual.

The probe needs only `git` on the remote — nothing is installed there. If the
cwd is not a repo the row simply shows nothing. Note the token names are
`mgit_*`, deliberately distinct from herdr-git-status's `$git_*`: if the
remote runs that plugin, its tokens reach your mirror rows through the
ordinary metadata forwarding, and both can coexist.

## Devcontainer

A herdr server running inside a container mirrors like any other host, reached
over `docker exec` instead of ssh. Nothing is installed into the container and
no port is published:

```toml
[hosts.dev]
kind = "docker"
folder = "/Users/you/code/my-project"   # devcontainer.local_folder label

# container = "my-container"            # ...or pin an explicit name instead
# docker_bin = "/usr/local/bin/docker"  # if docker isn't on the daemon's PATH
# remote_bin = "~/.local/bin/herdr"     # default; resolves inside the container,
                                        # so only set it if herdr lives elsewhere
# prefix = "dev"                        # sidebar prefix (default: the host key)
```

**Resolve by `folder`, not `container`.** Docker assigns a devcontainer a new
random name on every rebuild, so a pinned name breaks as soon as you rebuild.
The `devcontainer.local_folder` label is stable, and the mirror follows the
container across rebuilds.

**What the image needs:**

- **herdr**, running (`herdr status` inside the container should report a live
  server). This is the same requirement an ssh host has.
- **`socat`**, which bridges herdr's unix socket to `docker exec`'s stdio.
  Docker has no equivalent of ssh's `-L` socket forward, so one small relay
  process per API connection does that job. Missing socat is a clear error, not
  a silent degrade.

**A stopped container is not an error.** It reports as dormant and is retried
every 5 minutes rather than on the fast reconnect ladder, because "not running"
is a container's resting state — unlike an unreachable ssh host.

**Multiple containers** each get their own `[hosts.*]` entry and run
independently, exactly like multiple ssh hosts.

## Sidebar tokens

Custom metadata tokens the remote publishes are forwarded onto the mirror rows,
so a mirrored workspace or agent carries the same `$name` values a native one
does under whatever layout you configure locally. Useful for values herdr can't
derive from a mirror pane at all, like the remote working directory.

On the remote, report the value:

```bash
herdr pane report-metadata "$HERDR_PANE_ID" \
  --source user:rcwd --token "rcwd=$PWD" --ttl-ms 3600000
```

Locally, name it in a sidebar row (`~/.config/herdr/config.toml`):

```toml
[ui.sidebar.agents]
rows = [["state_icon", "workspace"], ["state_text", "agent"], ["$rcwd"]]
```

## Limitations

- **Version-locked to preview** until the `terminal session` streams reach
  stable; keep both ends on the same build.
- **Latency** above raw ssh: keystroke echo is a rendered frame round-trip, so
  there's a small constant delay. For latency-critical work, plain `ssh <host>`
  is always one command away.
- **No built-in branch chip on mirror rows** — herdr derives the sidebar's
  `branch`/`git_status` from the local workspace cwd, and there's no API to
  feed it a remote repo's state, so those built-ins stay blank on mirrors.
  The daemon bridges the gap itself with `$mgit_*` tokens (see
  [Git status on mirror rows](#git-status-on-mirror-rows)): one batched
  remote `git status` per host per interval, rendered by your sidebar rows.
  The remote's real branch and status stay visible in the streamed pane's
  prompt either way.
- **No custom sidebar UI** (plugin API limitation): mirrors carry a `<host>: `
  label prefix and the daemon keeps them ordered into per-host groups, but it
  can't render a richer affordance (group headers, collapse, colour).
- **Remote must be reachable and running herdr**; the daemon surfaces a
  readable status if a host is down or on too old a version.
- **ssh hosts whose sshd won't service streamlocal forwards** (some embedded
  Go sshds fronting container/VM workspaces accept the channel open and then
  never move a byte) fall back automatically to an exec relay — see
  `api_transport` above. The remote needs `socat` or `python3` for that path;
  almost everything has one or the other.

## License

MIT — see [LICENSE](./LICENSE).

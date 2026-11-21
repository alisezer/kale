# kale

Terminal setup for a headless home server that is driven from a Mac over SSH: one terminal tab is one tmux
session, and most sessions run Claude Code.

- **Session picker** (`src/`, Rust). Interactive SSH logins land in a picker. From there you attach to,
  start, rename or kill tmux sessions, grouped by project. `Alt+q` inside a session detaches back to it.
  Sessions are called `<project>-<n>`, and are renamed to `<project>-<slug>` after a Claude Code or Antigravity (`agy`) session rename.
- **tmux config** (`tmux/tmux.conf`). Status bar on top, tab titles that show the Claude Code / Antigravity session name and busy/idle status,
  extended keys for Shift+Enter, OSC 52 clipboard.
- **`bin/tmux-claude-title`**. Runs from the status bar (throttled to 4 s) and calls `kale tmux-titles` to
  put each Claude / Antigravity pane's name (and glyph for `agy`) into `@cc_name` and sync session renames.
- **Remote Control** (`systemd/`). A user timer runs `kale remote-up` to keep a Claude Remote Control
  server running for every project marked `remote = true`.
- **Mac side** (`mac/`). SSH aliases, a home-network check that picks LAN or Tailscale, and zsh shortcuts.

## Layout

| Path | Installed as | How |
|---|---|---|
| `src/`, `Cargo.toml` | `~/.local/bin/kale` | built and copied |
| `tmux/tmux.conf` | `~/.tmux.conf` | symlink |
| `bin/tmux-claude-title` | `~/.local/bin/tmux-claude-title` | symlink |
| `config/projects.toml` | `~/.config/kale/projects.toml` | symlink |
| `shell/kale.zsh` | sourced from `~/.zshrc` | source line |
| `systemd/kale-remote.{service,timer}` | `~/.config/systemd/user/` | copied |
| `mac/ssh_config` | the kale block of the Mac's `~/.ssh/config` | by hand |
| `mac/kale.zsh` | the Mac's `~/.zshrc` | by hand |
| `mac/at-home.sh` | the Mac's `~/.ssh/at-home.sh` | by hand; put your router's MAC in `~/.config/kale/home-gw-mac` |

## Install / update (server)

```sh
git clone https://github.com/alisezer/kale ~/code/kale
~/code/kale/install.sh
```

`install.sh` is safe to re-run. Any file it replaces is kept as `<file>.bak-<date>`.

## Editing

- The tmux config, title script and projects are symlinks, so edits take effect straight away.
  Reload tmux with `prefix r`, or run `tmux source-file ~/.tmux.conf`.
- After changing the picker, run `./install.sh`, or `cargo build --release && install -m755 target/release/kale ~/.local/bin/`.
- Projects live in `config/projects.toml`: `name`, `dir`, and optional `start = "claude"` and `remote = true`.

## Picker commands

```
kale                     picker: pick / start / kill sessions; detach (Alt+q) returns here
kale <project> [name]    start a new session in <project> (or attach <project>-<name>)
kale ls                  list sessions
kale attach <session>    attach by exact name
kale kill <session> [-y] kill by exact name
kale projects            list projects
kale remote-up           start missing Remote Control servers (run by kale-remote.timer)
```

## Notes

- tmux runs a status-bar `#()` job once per attached client, and every `tmux set` repaints every client in
  full. So `tmux-claude-title` only sets an option when its value changed, and runs at most once per 4 s.
  Without that, 6 attached tabs produced about 1.2 MB/s of full-screen repaints, which made typing over SSH lag.
- Sessions are always targeted by id (`$N`), never by name, because tmux matches name prefixes.
- To test against a throwaway tmux server, set `KALE_TMUX_SOCKET=<name>` (runs `tmux -L <name>`). Never test against the live server.

## License

MIT

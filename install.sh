#!/bin/sh
# Install kale from this checkout (run on kale). Safe to re-run.
#   - builds the picker and copies it to ~/.local/bin/kale
#   - symlinks tmux.conf, tmux-claude-title and projects.toml to this repo, so edits here are live
#   - installs the kale-remote user timer
#   - makes ~/.zshrc source shell/kale.zsh
# Anything it replaces is kept as <file>.bak-<date>.
set -eu
repo=$(cd "$(dirname "$0")" && pwd)
today=$(date +%F)

link() { # link <repo file> <target>
  src="$repo/$1"; dst=$2
  mkdir -p "$(dirname "$dst")"
  [ "$(readlink "$dst" 2>/dev/null)" = "$src" ] && return
  [ -e "$dst" ] && mv "$dst" "$dst.bak-$today"
  ln -s "$src" "$dst"
  echo "linked $dst -> $src"
}

(cd "$repo" && cargo build --release --locked)
install -m 755 "$repo/target/release/kale" ~/.local/bin/kale
echo "installed ~/.local/bin/kale"

link tmux/tmux.conf             ~/.tmux.conf
link bin/tmux-claude-title      ~/.local/bin/tmux-claude-title
link config/projects.toml       ~/.config/kale/projects.toml

mkdir -p ~/.config/systemd/user
install -m 644 "$repo"/systemd/kale-remote.service "$repo"/systemd/kale-remote.timer ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now kale-remote.timer >/dev/null

line='[[ -r ~/code/kale/shell/kale.zsh ]] && source ~/code/kale/shell/kale.zsh'
if ! grep -qF "$line" ~/.zshrc 2>/dev/null; then
  cp ~/.zshrc ~/.zshrc.bak-$today
  { echo "$line"; echo; cat ~/.zshrc.bak-$today; } > ~/.zshrc
  echo "added kale.zsh to ~/.zshrc"
fi

# Reload tmux config if a server is running
tmux source-file ~/.tmux.conf 2>/dev/null && echo "reloaded tmux config" || true
echo done

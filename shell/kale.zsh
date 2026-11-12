# Sourced at the top of kale's ~/.zshrc (install.sh adds the line).
# Interactive SSH logins open the kale session picker (~/.local/bin/kale): pick, start or
# kill tmux sessions; detaching returns to it. q closes the connection, s (or a picker error) leaves a plain shell.
# Skipped for non-interactive ssh commands, VS Code Remote-SSH, already inside tmux, or NO_TMUX=1.
if [[ -o interactive && -n $SSH_TTY && -z $TMUX && $TERM_PROGRAM != vscode && -z $NO_TMUX && -x ~/.local/bin/kale ]]; then
  ~/.local/bin/kale && exit
fi

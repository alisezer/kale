# kale session picker (~/.local/bin/kale on kale, source kale:~/code/kale). One Ghostty tab = one session.
#   kale                  picker: attach / new / kill sessions; Alt+q in a session returns to it
#   kale tuccar [name]    new tuccar session (or reattach tuccar-<name>)   kale ls   list sessions
#   tuccar / agents       shortcuts for kale tuccar / kale agents
kale() { if (( $# )); then ssh -t kale "~/.local/bin/kale ${(j: :)${(q)@}}"; else ssh kale; fi }
tuccar() { kale tuccar "$@"; }
agents() { kale agents "$@"; }

#!/usr/bin/env bash
set -euo pipefail

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
key="$(tmux show-option -gqv '@tmux-session-switch-key')"
width="$(tmux show-option -gqv '@tmux-session-switch-width')"
height="$(tmux show-option -gqv '@tmux-session-switch-height')"

key="${key:-T}"
width="${width:-90%}"
height="${height:-85%}"

bin="$CURRENT_DIR/target/release/tmux-session-switch"
if [[ ! -x "$bin" ]]; then
  bin="$CURRENT_DIR/bin/tmux-session-switch"
fi

tmux bind-key "$key" display-popup \
  -d '#{pane_current_path}' \
  -w "$width" \
  -h "$height" \
  -E "$bin"

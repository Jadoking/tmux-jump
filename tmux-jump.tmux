#!/usr/bin/env bash
set -euo pipefail

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
key="$(tmux show-option -gqv '@tmux-jump-key')"
width="$(tmux show-option -gqv '@tmux-jump-width')"
height="$(tmux show-option -gqv '@tmux-jump-height')"

key="${key:-T}"
width="${width:-90%}"
height="${height:-85%}"

bin="$CURRENT_DIR/target/release/tmux-jump"
if [[ ! -x "$bin" ]]; then
  bin="$CURRENT_DIR/bin/tmux-jump"
fi

tmux bind-key "$key" display-popup \
  -d '#{pane_current_path}' \
  -w "$width" \
  -h "$height" \
  -E "$bin"

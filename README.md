# tmux-jump

A fast Rust-powered tmux popup for switching sessions and launching new sessions from directories.

## Features

- `tmux display-popup` overlay
- Split layout:
  - top-left: existing tmux sessions
  - bottom-left: directory picker/search + new session name
  - right: preview/details
- Create a new tmux session from a selected directory
- Switch to existing sessions, ordered by recent activity
- Fuzzy search across session names, commands, and directories
- Rename or kill sessions from the popup
- Cached directory index with manual background refresh
- Scrollable live pane previews
- Adaptive monochrome UI that inherits terminal colors

## Requirements

- tmux 3.2+ for `display-popup`
- Rust toolchain for building from source

## Install with TPM

Add this to `~/.tmux.conf`:

```tmux
set -g @plugin 'Jadoking/tmux-jump'

# Optional settings
set -g @tmux-jump-key 'T'
set -g @tmux-jump-width '90%'
set -g @tmux-jump-height '85%'
```

Then press `prefix + I`.

The first launch builds the release binary automatically when Cargo is available. You can also build it explicitly:

```bash
cd ~/.tmux/plugins/tmux-jump
cargo build --release
```

Tagged releases provide binaries for Linux x86_64 and macOS Intel/Apple Silicon.

Reload tmux:

```bash
tmux source-file ~/.tmux.conf
```

Default binding: `prefix + T`.

## Manual install / build from source

```bash
git clone git@github.com:Jadoking/tmux-jump.git ~/dev/tmux-jump
cd ~/dev/tmux-jump
cargo build --release
```

Add to `~/.tmux.conf`:

```tmux
run-shell ~/dev/tmux-jump/tmux-jump.tmux
```

Reload tmux:

```bash
tmux source-file ~/.tmux.conf
```

## Usage

Open the popup: `prefix + T`.

Keys:

- `Up/Down`: move selection; overflow between session and directory boxes
- `Left/Right`: collapse/expand directories
- `Tab`: switch between directory search and new session name
- `Enter`: create or switch session
- `Ctrl-R`: rename selected session
- `Ctrl-D`: kill selected session with confirmation
- `Ctrl-N`: focus new-session input
- `Ctrl-U`: rebuild the directory cache in the background
- `PageUp/PageDown`: scroll the pane preview
- `Esc`: cancel an action or close the popup

## Directory cache

Directory search uses a bounded persistent cache:

```text
~/.cache/tmux-jump/dirs.txt
```

# tmux-session-switch

A fast Rust-powered tmux popup for switching sessions and launching new sessions from directories.

## Features

- `tmux display-popup` overlay
- Split layout:
  - top-left: directory picker/search + new session name
  - bottom-left: existing tmux sessions
  - right: preview/details
- Create a new tmux session from a selected directory
- Switch to existing sessions
- Cached directory index for fast searching
- Cyberdyne-inspired terminal UI

## Requirements

- tmux 3.2+ for `display-popup`
- Rust toolchain for building from source

Install Rust if needed:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

## Install with TPM

You do **not** need to register the plugin anywhere for TPM. TPM can install directly from GitHub.

Add this to `~/.tmux.conf`:

```tmux
set -g @plugin 'Jadoking/tmux-session-switch'

# Optional settings
set -g @tmux-session-switch-key 'T'
set -g @tmux-session-switch-width '90%'
set -g @tmux-session-switch-height '85%'
```

Then press:

```text
prefix + I
```

Build the binary after TPM installs it:

```bash
cd ~/.tmux/plugins/tmux-session-switch
cargo build --release
```

Reload tmux:

```bash
tmux source-file ~/.tmux.conf
```

Default binding:

```text
prefix + T
```

## Manual install / build from source

```bash
git clone git@github.com:Jadoking/tmux-session-switch.git ~/dev/tmux-session-switch
cd ~/dev/tmux-session-switch
cargo build --release
```

Add to `~/.tmux.conf`:

```tmux
run-shell ~/dev/tmux-session-switch/tmux-session-switch.tmux

# Optional settings
set -g @tmux-session-switch-key 'T'
set -g @tmux-session-switch-width '90%'
set -g @tmux-session-switch-height '85%'
```

Reload tmux:

```bash
tmux source-file ~/.tmux.conf
```

## Usage

Open the popup:

```text
prefix + T
```

Keys:

- `Up/Down`: move selection; overflow between directory and session boxes
- `Left/Right`: collapse/expand directories
- `Tab`: in the directory box, switch between directory search and new session name
- `Enter`: create/switch session from selected directory, or switch selected existing session
- `Esc`: close popup

## Directory cache

Directory search uses a bounded persistent cache:

```text
~/.cache/tmux-session-switch/dirs.txt
```

The cache is capped at 25,000 directories and refreshed in the background while the popup is open when stale. It excludes noisy/heavy directories like `.git`, `node_modules`, `target`, `Library`, and caches.

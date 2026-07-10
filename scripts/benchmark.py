#!/usr/bin/env python3
"""Compare tmux-jump binaries in an isolated tmux/filesystem fixture."""

from __future__ import annotations

import argparse
import errno
import fcntl
import json
import os
import pty
import select
import statistics
import struct
import subprocess
import tempfile
import shutil
import termios
import time
from pathlib import Path


def make_fixture(root: Path, session_count: int, directory_count: int) -> dict[str, str]:
    home = root / "home"
    cache = root / "cache"
    fixture_bin = root / "bin"
    home.mkdir()
    cache.mkdir()
    fixture_bin.mkdir()

    real_tmux = shutil.which("tmux")
    if real_tmux is None:
        raise RuntimeError("tmux is not installed")
    socket = root / "tmux.sock"
    wrapper = fixture_bin / "tmux"
    wrapper.write_text(
        "#!/bin/sh\n"
        f"exec {real_tmux!r} -S {str(socket)!r} \"$@\"\n"
    )
    wrapper.chmod(0o755)

    directories = [home]
    for i in range(directory_count):
        directory = home / f"project-{i:04d}"
        directory.mkdir()
        (directory / "src").mkdir()
        directories.extend((directory, directory / "src"))

    cache_file = cache / "tmux-jump" / "dirs.txt"
    cache_file.parent.mkdir(parents=True)
    cache_file.write_text("".join(f"{path}\n" for path in directories))

    env = os.environ.copy()
    env.pop("TMUX", None)
    env.pop("TMUX_PANE", None)
    env.update(
        HOME=str(home),
        XDG_CACHE_HOME=str(cache),
        PATH=f"{fixture_bin}{os.pathsep}{env.get('PATH', '')}",
        TERM="xterm-256color",
    )
    for i in range(session_count):
        args = ["tmux"]
        if i == 0:
            args += ["-f", "/dev/null"]
        args += ["new-session", "-d", "-s", f"bench-{i:03d}", "sleep 300"]
        subprocess.run(args, env=env, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return env


def drain(fd: int) -> None:
    while True:
        try:
            if not os.read(fd, 65536):
                return
        except BlockingIOError:
            return
        except OSError as error:
            if error.errno == errno.EIO:
                return
            raise


def run_once(binary: Path, env: dict[str, str], idle_seconds: float) -> dict[str, float]:
    pid, master = pty.fork()
    if pid == 0:
        os.execve(binary, [str(binary)], env)

    fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 160, 0, 0))
    fcntl.fcntl(master, fcntl.F_SETFL, os.O_NONBLOCK)
    started = time.perf_counter()
    deadline = started + idle_seconds
    while time.perf_counter() < deadline:
        timeout = min(0.01, max(0.0, deadline - time.perf_counter()))
        select.select([master], [], [], timeout)
        drain(master)

    os.write(master, b"\x1b")
    while True:
        drain(master)
        waited_pid, status, usage = os.wait4(pid, os.WNOHANG)
        if waited_pid:
            break
        select.select([master], [], [], 0.01)
    wall = time.perf_counter() - started
    os.close(master)
    if not os.WIFEXITED(status) or os.WEXITSTATUS(status) != 0:
        raise RuntimeError(f"{binary} exited with status {status}")
    return {"wall_ms": wall * 1000, "cpu_ms": (usage.ru_utime + usage.ru_stime) * 1000}


def summarize(samples: list[dict[str, float]]) -> dict[str, float]:
    return {
        "wall_ms_median": round(statistics.median(s["wall_ms"] for s in samples), 2),
        "cpu_ms_median": round(statistics.median(s["cpu_ms"] for s in samples), 2),
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("binaries", nargs="+", type=Path)
    parser.add_argument("--runs", type=int, default=10)
    parser.add_argument("--sessions", type=int, default=20)
    parser.add_argument("--directories", type=int, default=100)
    parser.add_argument("--idle-seconds", type=float, default=0.5)
    args = parser.parse_args()
    binaries = [binary.resolve() for binary in args.binaries]

    with tempfile.TemporaryDirectory(prefix="tmux-jump-bench-") as temp:
        root = Path(temp)
        env = make_fixture(root, args.sessions, args.directories)
        try:
            results: dict[str, dict[str, dict[str, float]]] = {}
            for binary in binaries:
                for idle in (0.0, args.idle_seconds):
                    run_once(binary, env, idle)  # warmup
                startup = [run_once(binary, env, 0.0) for _ in range(args.runs)]
                idle = [run_once(binary, env, args.idle_seconds) for _ in range(args.runs)]
                results[binary.name] = {
                    "startup": summarize(startup),
                    f"idle_{args.idle_seconds:g}s": summarize(idle),
                }
            print(json.dumps(results, indent=2))
        finally:
            subprocess.run(["tmux", "kill-server"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""SAG 开发环境启动脚本 — 一键启动 Rust 后端 + Vue 前端.

用法:
    python3 scripts/dev.py                    # 默认配置
    python3 scripts/dev.py --config my.toml   # 指定配置文件
    python3 scripts/dev.py --skip-ui          # 只启动后端
    python3 scripts/dev.py --skip-backend     # 只启动前端
    python3 scripts/dev.py --release          # release 模式启动后端
"""

import argparse
import os
import select
import signal
import subprocess
import threading
import time
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parent.parent


def log(tag: str, msg: str) -> None:
    """带时间戳的彩色日志."""
    colors = {"BACKEND": "\033[36m", "UI": "\033[35m", "SYS": "\033[33m"}
    reset = "\033[0m"
    c = colors.get(tag, "")
    ts = time.strftime("%H:%M:%S")
    print(f"[{ts}] {c}[{tag}]{reset} {msg}", flush=True)


def start_backend(config: str, release: bool) -> subprocess.Popen:
    """使用 watchexec 启动 Rust 后端，源码变化时自动重编译重启."""
    cmd = ["watchexec", "-r", "--", "cargo", "run"]
    if release:
        cmd.append("--release")
    cmd.extend(["--", config])

    log("SYS", f"Starting backend: {' '.join(cmd)}")
    return subprocess.Popen(
        cmd,
        cwd=PROJECT_ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
        start_new_session=True,
    )


def start_ui() -> subprocess.Popen:
    """启动 Vue 前端开发服务器."""
    ui_dir = PROJECT_ROOT / "sag-ui"
    if not (ui_dir / "node_modules").exists():
        log("SYS", "Installing UI dependencies first...")
        subprocess.run(["npm", "install"], cwd=ui_dir, check=True)

    log("SYS", "Starting UI dev server: npm run dev")
    return subprocess.Popen(
        ["npm", "run", "dev"],
        cwd=ui_dir,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
        start_new_session=True,
    )


def stream_output(proc: subprocess.Popen, tag: str) -> None:
    """将子进程输出实时打印到终端."""
    assert proc.stdout is not None
    for line in proc.stdout:
        log(tag, line.rstrip())


def kill_process_group(proc: subprocess.Popen, sig: int) -> None:
    """Send a signal to the entire process group of the child process."""
    try:
        os.killpg(os.getpgid(proc.pid), sig)
    except (ProcessLookupError, OSError):
        try:
            proc.send_signal(sig)
        except ProcessLookupError:
            pass  # already exited


def shutdown(procs: list[subprocess.Popen]) -> None:
    """Gracefully terminate all child processes and their descendants."""
    log("SYS", "Shutting down...")

    for p in procs:
        if p.poll() is not None:
            continue  # already exited

        # Step 1: SIGTERM to the entire process group
        kill_process_group(p, signal.SIGTERM)

        # Step 2: Wait up to 5 seconds for graceful exit
        try:
            p.wait(timeout=5)
        except subprocess.TimeoutExpired:
            # Step 3: Force-kill with SIGKILL
            kill_process_group(p, signal.SIGKILL)
            try:
                p.wait(timeout=3)
            except subprocess.TimeoutExpired:
                log("SYS", f"WARNING: process {p.pid} unkillable")

    log("SYS", "All processes stopped. Bye!")


def wait_for_processes(procs: list[subprocess.Popen]) -> None:
    """Block until all processes exit or SIGINT/SIGTERM is received.

    Uses select.select() instead of p.wait(timeout=N) because the latter
    is not reliably interruptible by signals on macOS (CPython's subprocess
    module uses a C-level waitpid loop that may retry on EINTR).
    select.select() cleanly raises InterruptedError on signal delivery,
    allowing us to catch it and trigger a graceful shutdown.
    """
    shutdown_requested = False

    def on_signal(signum: int, _frame: object) -> None:
        nonlocal shutdown_requested
        shutdown_requested = True

    # Temporarily install handlers that just set a flag
    prev_int = signal.signal(signal.SIGINT, on_signal)
    prev_term = signal.signal(signal.SIGTERM, on_signal)

    try:
        while not shutdown_requested:
            # check if all children have exited
            if all(p.poll() is not None for p in procs):
                break

            # Sleep in an interruptible way — select on no FDs with a
            # timeout. On SIGINT/SIGTERM, this raises InterruptedError.
            try:
                select.select([], [], [], 0.5)
            except InterruptedError:
                continue  # signal handler sets shutdown_requested, loop exits

    finally:
        # Restore original handlers
        signal.signal(signal.SIGINT, prev_int)
        signal.signal(signal.SIGTERM, prev_term)

    if shutdown_requested:
        shutdown(procs)


def main() -> None:
    parser = argparse.ArgumentParser(description="SAG 开发环境启动器")
    parser.add_argument("--config", default="gateway.toml")
    parser.add_argument("--release", action="store_true")
    parser.add_argument("--skip-backend", action="store_true")
    parser.add_argument("--skip-ui", action="store_true")
    args = parser.parse_args()

    procs: list[subprocess.Popen] = []

    print()
    log("SYS", "╔══════════════════════════════════════╗")
    log("SYS", "║   SAG — Smart AI Gateway Dev Mode    ║")
    log("SYS", "╚══════════════════════════════════════╝")
    print()

    # ── 后端 ──
    if not args.skip_backend:
        backend = start_backend(args.config, args.release)
        procs.append(backend)
        threading.Thread(target=stream_output, args=(backend, "BACKEND"), daemon=True).start()

    # ── 前端 ──
    if not args.skip_ui:
        ui = start_ui()
        procs.append(ui)
        threading.Thread(target=stream_output, args=(ui, "UI"), daemon=True).start()

    if not procs:
        log("SYS", "Nothing to start (both --skip-backend and --skip-ui)")
        return

    log("SYS", "Gateway API: http://127.0.0.1:3000")
    log("SYS", "Dashboard UI: http://localhost:5173")
    log("SYS", "Press Ctrl+C to stop both processes.")
    print()

    wait_for_processes(procs)


if __name__ == "__main__":
    main()

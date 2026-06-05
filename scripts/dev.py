#!/usr/bin/env python3
"""SAG 开发环境启动脚本 — 一键启动 Rust 后端 + Vue 前端.

用法:
    python3 dev.py                    # 默认配置
    python3 dev.py --config my.toml   # 指定配置文件
    python3 dev.py --skip-ui          # 只启动后端
    python3 dev.py --skip-backend     # 只启动前端
    python3 dev.py --release          # release 模式启动后端
"""

import argparse
import os
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parent.parent
BACKEND_BIN = "sag-gateway"  # cargo crate name (not the binary)


def log(tag: str, msg: str) -> None:
    """带时间戳的彩色日志."""
    colors = {"BACKEND": "\033[36m", "UI": "\033[35m", "SYS": "\033[33m"}
    reset = "\033[0m"
    c = colors.get(tag, "")
    ts = time.strftime("%H:%M:%S")
    print(f"[{ts}] {c}[{tag}]{reset} {msg}", flush=True)


def start_backend(config: str, release: bool) -> subprocess.Popen:
    """启动 Rust 后端进程."""
    profile = "--release" if release else ""
    cmd = f"cargo run {profile} -- {config}".strip().split()
    log("SYS", f"Starting backend: {' '.join(cmd)}")
    return subprocess.Popen(
        cmd,
        cwd=PROJECT_ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
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
    )


def stream_output(proc: subprocess.Popen, tag: str) -> None:
    """将子进程输出实时打印到终端."""
    for line in proc.stdout:  # type: ignore[union-attr]
        log(tag, line.rstrip())


def main() -> None:
    parser = argparse.ArgumentParser(description="SAG 开发环境启动器")
    parser.add_argument("--config", default="gateway.toml", help="配置文件路径 (默认: gateway.toml)")
    parser.add_argument("--release", action="store_true", help="使用 release 模式编译后端")
    parser.add_argument("--skip-backend", action="store_true", help="跳过后端启动")
    parser.add_argument("--skip-ui", action="store_true", help="跳过前端启动")
    args = parser.parse_args()

    procs: list[subprocess.Popen] = []
    threads: list[threading.Thread] = []

    def cleanup(*_args) -> None:
        """优雅关闭所有子进程."""
        log("SYS", "Shutting down...")
        for p in procs:
            if p.poll() is None:
                p.terminate()
                try:
                    p.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    p.kill()
                    p.wait()
        log("SYS", "All processes stopped. Bye!")

    signal.signal(signal.SIGINT, cleanup)
    signal.signal(signal.SIGTERM, cleanup)

    print()
    log("SYS", "╔══════════════════════════════════════╗")
    log("SYS", "║   SAG — Smart AI Gateway Dev Mode    ║")
    log("SYS", "╚══════════════════════════════════════╝")
    print()

    # ── 后端 ──
    if not args.skip_backend:
        log("SYS", "Building backend (this may take a moment)...")
        build_result = subprocess.run(
            ["cargo", "build"] + (["--release"] if args.release else []),
            cwd=PROJECT_ROOT,
            capture_output=True,
            text=True,
        )
        if build_result.returncode != 0:
            log("SYS", "Backend build FAILED:")
            print(build_result.stderr)
            sys.exit(1)
        log("SYS", "Backend build OK")

        backend = start_backend(args.config, args.release)
        procs.append(backend)
        t = threading.Thread(target=stream_output, args=(backend, "BACKEND"), daemon=True)
        t.start()
        threads.append(t)

    # ── 前端 ──
    if not args.skip_ui:
        ui = start_ui()
        procs.append(ui)
        t = threading.Thread(target=stream_output, args=(ui, "UI"), daemon=True)
        t.start()
        threads.append(t)

    if not procs:
        log("SYS", "Nothing to start (both --skip-backend and --skip-ui)")
        return

    log("SYS", f"Gateway API: http://127.0.0.1:3000")
    log("SYS", f"Dashboard UI: http://localhost:5173")
    log("SYS", "Press Ctrl+C to stop both processes.")
    print()

    # 等待所有子进程结束
    try:
        for p in procs:
            p.wait()
    except KeyboardInterrupt:
        cleanup()


if __name__ == "__main__":
    main()

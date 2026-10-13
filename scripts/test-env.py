#!/usr/bin/env python3
"""SAG test-env launcher — keeps a release sag binary running in steady state.

Subcommands
-----------
start
    Spawn `./target/release/sag gateway.test.toml` detached, write
    `.test-env/{pid, build-info.json}`. The sag binary follows the XDG
    Base Directory spec (sag-core/src/paths.rs); the launcher overrides
    XDG_CONFIG_HOME / XDG_DATA_HOME / XDG_STATE_HOME to project-local
    `.test-env/xdg/{config,data,state}` so the test track is physically
    isolated from the developer's user-level XDG directories:
    the analytics DB lands in `.test-env/xdg/data/sag/` and the sag
    binary writes its own logs to daily-rotating files under
    `.test-env/xdg/state/sag/logs/` (e.g.
    `.test-env/xdg/state/sag/logs/sag.log.2026-06-26`) via
    tracing-appender. stdout is redirected to `os.devnull` (the binary
    does file-based logging), while stderr is captured to
    `.test-env/stderr.log` so panics and non-tracing `Error:` output
    leave a trace instead of vanishing silently.
    Locks are acquired atomically (O_EXCL) so two concurrent `start`
    calls cannot both proceed past the lock check. On any failure during
    start, lock + pid + build-info are removed.

stop
    Read `.test-env/pid` → SIGTERM the recorded PID, wait up to 5s for
    graceful exit, then SIGKILL. Cleans up lock + pid + build-info.

restart
    stop + start. If start fails, also clean up state. `restart` tolerates
    a missing pid file (nothing to stop) so it can bring up a fresh
    instance even when no prior service exists.

status (default when no subcommand given)
    Print pid / started_at / uptime / binary mtime + size / config path +
    config mtime / last 20 log lines (from the latest rolling log file).
    Exits 1 if no `.test-env/pid`.

This script does NOT trigger compilation. To rebuild the binary:
    just build-test

State under `.test-env/`:
    pid           — text file containing the current SAG process PID
                     (canonical pointer; written BEFORE build-info.json)
    lock          — created atomically with O_EXCL; signals "instance owned"
    build-info.json — machine-readable summary of the launch
    xdg/          — XDG_* override root (config / data / state); the whole
                     tree is gitignored alongside `.test-env/`

Logs:
    .test-env/xdg/state/sag/logs/sag.log.YYYY-MM-DD — daily-rotating,
        managed by tracing-appender (under the launcher's XDG_STATE_HOME
        override)
    .test-env/stderr.log — captured stderr of the sag process (overwritten
        on every start), for panics / non-tracing errors
"""

import argparse
import json
import os
import signal
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

PROJECT_ROOT = Path(__file__).resolve().parent.parent
TEST_ENV_DIR = PROJECT_ROOT / ".test-env"
# XDG_* override root injected into the sag child process (see module
# docstring): keeps config / data / state physically inside the project.
XDG_ROOT = TEST_ENV_DIR / "xdg"
# The sag binary resolves its log dir as `$XDG_STATE_HOME/sag/logs`
# (sag-core/src/paths.rs → default_log_dir).
LOGS_DIR = XDG_ROOT / "state" / "sag" / "logs"
PID_FILE = TEST_ENV_DIR / "pid"
LOCK_FILE = TEST_ENV_DIR / "lock"
BUILD_INFO = TEST_ENV_DIR / "build-info.json"
STDERR_LOG = TEST_ENV_DIR / "stderr.log"
# Daily-rotated log files are named `sag.log.YYYY-MM-DD` (tracing-appender
# prefixes the file with "sag.log" and appends the date).
LOG_GLOB = ".test-env/xdg/state/sag/logs/sag.log.*"

DEFAULT_BINARY = PROJECT_ROOT / "target" / "release" / "sag"
DEFAULT_CONFIG = PROJECT_ROOT / "gateway.test.toml"

SIGTERM_GRACE_SECS = 5

# ── logging ───────────────────────────────────────────────────────────────

_COLORS = {"SYS": "\033[36m", "OK": "\033[32m", "ERR": "\033[31m"}
_RESET = "\033[0m"


def log(tag: str, msg: str) -> None:
    if sys.stdout.isatty():
        c = _COLORS.get(tag, "")
        print(f"{c}[{tag}]{_RESET} {msg}", flush=True)
    else:
        print(f"[{tag}] {msg}", flush=True)


# ── time helpers ──────────────────────────────────────────────────────────


def now_rfc3339() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def file_mtime_rfc3339(path: Path) -> str:
    return datetime.fromtimestamp(path.stat().st_mtime, timezone.utc).strftime(
        "%Y-%m-%dT%H:%M:%SZ"
    )


# ── state helpers ─────────────────────────────────────────────────────────


def ensure_state_dirs() -> None:
    TEST_ENV_DIR.mkdir(parents=True, exist_ok=True)
    LOGS_DIR.mkdir(parents=True, exist_ok=True)


def child_env() -> dict:
    """Environment for the sag child process with XDG_* overridden to the
    project-local `.test-env/xdg/` tree, so the test track never touches the
    developer's user-level XDG directories (physical isolation, mirroring
    the dev/test track split in AGENTS.md)."""
    env = os.environ.copy()
    env["XDG_CONFIG_HOME"] = str(XDG_ROOT / "config")
    env["XDG_DATA_HOME"] = str(XDG_ROOT / "data")
    env["XDG_STATE_HOME"] = str(XDG_ROOT / "state")
    return env


def pid_alive(pid: int) -> bool:
    """Return True iff `pid` exists. Used for stale-state detection."""
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        # Owned by another user but exists; treat as not-our-process.
        return False


def _acquire_lock() -> None:
    """Atomically create the lock file, or exit 1 if it already exists."""
    try:
        fd = os.open(str(LOCK_FILE), os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o644)
        os.close(fd)
    except FileExistsError:
        log(
            "ERR",
            f"lock file {LOCK_FILE} already exists. Either another `start` is "
            "in progress, or a previous run was interrupted. Inspect "
            "`.test-env/` and either run `stop` or remove stale files.",
        )
        sys.exit(1)


# ── subcommands ───────────────────────────────────────────────────────────


def cmd_start(args: argparse.Namespace) -> None:
    ensure_state_dirs()

    # 1. Pre-check: a recorded pid that's still alive means a real instance.
    if PID_FILE.exists():
        try:
            existing = int(PID_FILE.read_text().strip())
        except (ValueError, OSError):
            existing = None
        if existing is not None and pid_alive(existing):
            log(
                "ERR",
                f"test-env already running with PID {existing}. "
                "Run `python3 scripts/test-env.py stop` first "
                "(or `just test-env-stop`).",
            )
            sys.exit(1)
        log("SYS", "previous pid file was stale; clearing")
        PID_FILE.unlink(missing_ok=True)

    # 2. Resolve binary + config; bail with a clear message on missing file.
    binary = Path(args.binary) if args.binary else DEFAULT_BINARY
    config = Path(args.config) if args.config else DEFAULT_CONFIG

    if not binary.exists():
        log(
            "ERR",
            f"binary not found: {binary}\n"
            "       did you run `just build-test`?",
        )
        sys.exit(1)
    if not config.exists():
        log(
            "ERR",
            f"config not found: {config}\n"
            "       copy gateway.test.example.toml to gateway.test.toml "
            "and fill in keys.",
        )
        sys.exit(1)

    # 3. Atomic lock — concurrent `start` invocations can't both pass.
    _acquire_lock()

    try:
        # 4. Spawn detached. The sag binary writes its own logs to
        #    daily-rotating files under `logs/` via tracing-appender;
        #    we redirect stdout to DEVNULL so the launcher does
        #    not interfere with file-based logging. stderr is captured
        #    to `.test-env/stderr.log` (truncated on each start) so
        #    panics / non-tracing errors are never silently lost.
        #    start_new_session=True puts sag in its own process group
        #    so SIGTERM targets only sag.
        stderr_handle = open(STDERR_LOG, "w")
        proc = subprocess.Popen(
            [str(binary), str(config)],
            cwd=PROJECT_ROOT,
            env=child_env(),
            stdout=subprocess.DEVNULL,
            stderr=stderr_handle,
            stdin=subprocess.DEVNULL,
            start_new_session=True,
        )

        # 5. Write the canonical pointer (pid file) FIRST. After this point,
        #    `stop` and `status` will see sag as "real". build-info.json is
        #    metadata; partial-write here would still leave pid_file pointing
        #    at the live process.
        PID_FILE.write_text(str(proc.pid))
        # Close the parent's copy of the stderr fd; the child keeps its own,
        #    so the file remains writable by the sag process.
        stderr_handle.close()

        # 6. Now write build-info.json with the pid resolved.
        info = {
            "pid": proc.pid,
            "started_at": now_rfc3339(),
            "binary_path": str(binary.resolve()),
            "binary_mtime": file_mtime_rfc3339(binary),
            "binary_size_bytes": binary.stat().st_size,
            "config_path": str(config.resolve()),
            "config_mtime": file_mtime_rfc3339(config),
        }
        BUILD_INFO.write_text(json.dumps(info, indent=2))

        log("OK", f"test-env launched: PID {proc.pid}")
        log("SYS", f"binary: {binary}")
        log("SYS", f"config: {config}")
        log("SYS", f"logs:   {LOGS_DIR}/sag.log.DATE (daily rotating)")
        log("SYS", f"stderr: {STDERR_LOG}")
        log("SYS", f"info:   {BUILD_INFO}")
    except Exception:
        log("ERR", "start failed; cleaning up lock / pid / build-info")
        _cleanup_state()
        raise


def cmd_stop(
    _args: Optional[argparse.Namespace] = None, allow_missing: bool = True
) -> None:
    if not PID_FILE.exists():
        if allow_missing:
            # A lock without a pid is always orphaned (the pid is the canonical
            # pointer). Clear stale lock/build-info so the next `start` isn't
            # blocked by a leftover lock from an interrupted run.
            if LOCK_FILE.exists() or BUILD_INFO.exists():
                log("SYS", "no pid file; clearing orphaned lock/build-info")
                _cleanup_state()
            else:
                log("SYS", "no pid file - nothing to stop (continuing)")
            return
        log("ERR", "no pid file - nothing to stop")
        sys.exit(1)

    try:
        pid = int(PID_FILE.read_text().strip())
    except (ValueError, OSError) as exc:
        log("ERR", f"corrupt pid file: {exc}")
        _cleanup_state()
        if allow_missing:
            return
        sys.exit(1)

    if not pid_alive(pid):
        log("SYS", f"PID {pid} not running — clearing stale state")
        _cleanup_state()
        return

    log("SYS", f"SIGTERM -> PID {pid}")
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        log("SYS", f"PID {pid} exited before SIGTERM was delivered")
        _cleanup_state()
        return

    deadline = time.monotonic() + SIGTERM_GRACE_SECS
    while time.monotonic() < deadline:
        if not pid_alive(pid):
            log("OK", f"PID {pid} exited gracefully")
            _cleanup_state()
            return
        time.sleep(0.2)

    log("SYS", f"PID {pid} still alive after {SIGTERM_GRACE_SECS}s; SIGKILL")
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass

    # SIGKILL is uninterceptible, but allow the kernel a moment to reap.
    time.sleep(0.5)
    _cleanup_state()
    log("OK", f"PID {pid} stopped")


def _cleanup_state() -> None:
    PID_FILE.unlink(missing_ok=True)
    LOCK_FILE.unlink(missing_ok=True)
    BUILD_INFO.unlink(missing_ok=True)


def cmd_restart(args: argparse.Namespace) -> None:
    log("SYS", "restart: stopping existing instance (if any)")
    cmd_stop(allow_missing=True)
    log("SYS", "restart: starting fresh instance")
    cmd_start(args)


def cmd_status(_args: Optional[argparse.Namespace] = None) -> None:
    # 1. pid file is the canonical pointer; missing means no instance.
    if not PID_FILE.exists():
        log("ERR", "no test-env state (`.test-env/pid` missing)")
        sys.exit(1)

    try:
        pid = int(PID_FILE.read_text().strip())
    except (ValueError, OSError) as exc:
        log("ERR", f"corrupt pid file: {exc}")
        sys.exit(1)

    alive = pid_alive(pid)

    # 2. build-info.json is best-effort metadata. Treat missing/unreadable
    #    as "partial state" rather than failing the whole status read.
    info = None
    if BUILD_INFO.exists():
        try:
            info = json.loads(BUILD_INFO.read_text())
        except (OSError, json.JSONDecodeError):
            pass

    print("== test-env status ==")
    print(f"PID:        {pid}  ({'alive' if alive else 'DEAD'})")

    if info is None:
        print("(build-info.json unavailable — partial state)")
    else:
        try:
            started = datetime.fromisoformat(
                info["started_at"].replace("Z", "+00:00")
            )
            uptime_secs = max(
                0.0, (datetime.now(timezone.utc) - started).total_seconds()
            )
            h = int(uptime_secs // 3600)
            m = int((uptime_secs % 3600) // 60)
            s = int(uptime_secs % 60)
            print(f"Started:    {info['started_at']}")
            print(f"Uptime:     {h:02d}:{m:02d}:{s:02d}")
        except (KeyError, ValueError):
            print("(build-info.json started_at unparseable)")
        print(f"Binary:     {info.get('binary_path', '?')}")
        if "binary_mtime" in info:
            print(f"  mtime:    {info['binary_mtime']}")
        if "binary_size_bytes" in info:
            print(f"  size:     {info['binary_size_bytes']} bytes")
        print(f"Config:     {info.get('config_path', '?')}")
        if "config_mtime" in info:
            print(f"  mtime:    {info['config_mtime']}")

    # 3. Find the latest rolling log file (logs/sag.log.*).
    log_files = sorted(
        PROJECT_ROOT.glob(LOG_GLOB),
        key=lambda p: p.stat().st_mtime,
        reverse=True,
    )
    latest_log = log_files[0] if log_files else None

    print()
    # stderr capture: surface it if present (e.g. a startup panic).
    if STDERR_LOG.exists():
        stderr_size = STDERR_LOG.stat().st_size
        if stderr_size:
            print(f"== stderr ({STDERR_LOG}, {stderr_size} bytes) ==")
            try:
                with open(STDERR_LOG, "rb") as err_f:
                    err_f.seek(0, os.SEEK_END)
                    err_f.seek(max(0, err_f.tell() - 4096))
                    tail = err_f.read().decode("utf-8", errors="replace").splitlines()[-20:]
                for line in tail:
                    print(f"  {line}")
            except OSError as exc:
                print(f"  (could not read: {exc})")
        print()
    if latest_log:
        print(f"== log tail (last 20 of {latest_log}) ==")
        try:
            with open(latest_log, "rb") as log_f:
                log_f.seek(0, os.SEEK_END)
                size = log_f.tell()
                log_f.seek(max(0, size - 4096))
                tail = log_f.read().decode("utf-8", errors="replace").splitlines()[-20:]
            for line in tail:
                print(f"  {line}")
        except OSError as exc:
            print(f"  (could not read: {exc})")
    else:
        print(f"  (no log files matching {LOG_GLOB})")


# ── entrypoint ────────────────────────────────────────────────────────────


def main() -> None:
    parser = argparse.ArgumentParser(description="SAG test-env launcher")
    sub = parser.add_subparsers(dest="cmd")

    p_start = sub.add_parser("start", help="spawn sag binary (rejects duplicate)")
    p_start.add_argument(
        "--binary", help=f"override binary path (default: {DEFAULT_BINARY})"
    )
    p_start.add_argument(
        "--config", help=f"override config path (default: {DEFAULT_CONFIG})"
    )

    sub.add_parser("stop", help="SIGTERM (5s grace) → SIGKILL the running test-env")

    p_restart = sub.add_parser("restart", help="stop + start (atomic on failure)")
    p_restart.add_argument("--binary")
    p_restart.add_argument("--config")

    sub.add_parser("status", help="print pid / uptime / binary metadata / log tail")

    args = parser.parse_args()

    dispatch = {
        "start": cmd_start,
        "stop": cmd_stop,
        "restart": cmd_restart,
        "status": cmd_status,
    }
    if args.cmd not in dispatch:
        # Default to status when called with no subcommand.
        cmd_status()
        return
    dispatch[args.cmd](args)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Real-PTY TUI smoke test; requires Python 3.11+, Unix, and a built binary.

Usage: python3 scripts/tui-smoke.py [--binary target/debug/cc-switch-lite]
Creates a temporary configuration, drives the provider editor, and checks masked
credentials, private permissions and terminal restoration. Persistent run metadata
and a credential-free result are written under .codex/runs/tui-<UTC>-<PID>/.
"""
import argparse
import datetime
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import pty
import select
import struct
import subprocess
import tempfile
import termios
import time
import tomllib


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/debug/cc-switch-lite")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    binary = (root / args.binary).resolve()
    if not binary.is_file():
        parser.error("Binary missing; run cargo build --locked first")
    run = root / ".codex/runs" / (
        "tui-" + datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%S") + f"-{os.getpid()}"
    )
    run.mkdir(parents=True)
    metadata = {
        "python": platform.python_version(), "platform": platform.platform(),
        "binary": str(binary), "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "script_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "terminal": "xterm-256color", "rows": 30, "columns": 140, "timeout_seconds": 15,
        "git_head": subprocess.run(["git", "rev-parse", "HEAD"], cwd=root, capture_output=True, text=True).stdout.strip(),
        "git_status": subprocess.run(["git", "status", "--short"], cwd=root, capture_output=True, text=True).stdout,
    }
    (run / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    master, slave = pty.openpty()
    original = termios.tcgetattr(slave)
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 140, 0, 0))
    captured = bytearray()
    process = None
    deadline = time.monotonic() + 15

    def drain_until(predicate):
        while not predicate():
            if time.monotonic() >= deadline:
                raise AssertionError("TUI smoke timed out")
            if process.poll() is not None:
                raise AssertionError("TUI exited before expected state")
            ready, _, _ = select.select([master], [], [], 0.1)
            if ready:
                captured.extend(os.read(master, 65536))
                # Crossterm may request the cursor position during startup.
                if b"\x1b[6n" in captured:
                    os.write(master, b"\x1b[1;1R")
                    captured[:] = captured.replace(b"\x1b[6n", b"")

    try:
        with tempfile.TemporaryDirectory(prefix="tui-config-", dir=run) as directory:
            config = Path(directory) / "config.toml"
            process = subprocess.Popen(
                [str(binary), "tui", "--config", str(config)], cwd=root,
                stdin=slave, stdout=slave, stderr=slave,
                env={**os.environ, "TERM": "xterm-256color"},
            )
            drain_until(lambda: b"Providers" in captured)
            os.write(master, b"a")
            drain_until(lambda: b"Edit provider" in captured)
            secret = b"smoke-private-key-51927"
            os.write(master, b"smoke\tSmoke Provider\thttps://example.com/v1\t" + secret + b"\x13")
            drain_until(lambda: config.exists())
            content = tomllib.loads(config.read_text())
            assert content["default_route"]["provider"] == "smoke", "Default route not selected"
            assert "route" not in content and "routes" not in content, "Empty legacy fields persisted"
            assert content["providers"][0] == {
                "id": "smoke", "name": "Smoke Provider", "protocol": "responses",
                "base_url": "https://example.com/v1", "api_key": secret.decode(),
                "compat": {"meta": {}, "settings_config": {}},
            }, "Saved provider differs from input"
            assert config.stat().st_mode & 0o777 == 0o600, "Configuration permissions not private"
            os.write(master, b"\ta")
            # Ratatui draws only changed cells: a reused "Edit " prefix need not
            # appear again in the PTY byte stream. Verify the saved state below.
            os.write(master, b"claude\tclaude-*\tsmoke\x13")
            drain_until(lambda: len(tomllib.loads(config.read_text()).get("routes", [])) == 1)
            assert tomllib.loads(config.read_text())["routes"][0] == {
                "name": "claude", "model_pattern": "claude-*", "provider": "smoke",
            }, "Route editor did not persist the rule"
            # Add a second rule, reorder it, then delete it through the confirmation.
            os.write(master, b"aother\tgpt-*\tsmoke\x13")
            drain_until(lambda: len(tomllib.loads(config.read_text()).get("routes", [])) == 2)
            os.write(master, b"J")
            drain_until(lambda: tomllib.loads(config.read_text())["routes"][0]["name"] == "other")
            os.write(master, b"kdy")
            drain_until(lambda: len(tomllib.loads(config.read_text())["routes"]) == 1)
            # Edit the remaining rule's name and clear the default.
            os.write(master, b"e\x15renamed\x13")
            drain_until(lambda: tomllib.loads(config.read_text())["routes"][0]["name"] == "renamed")
            os.write(master, b"x")
            drain_until(lambda: "default_route" not in tomllib.loads(config.read_text()))
            os.write(master, b"q")
            process.wait(timeout=max(0.1, deadline - time.monotonic()))
            while select.select([master], [], [], 0)[0]:
                captured.extend(os.read(master, 65536))
            assert process.returncode == 0, "TUI exit failed"
            assert termios.tcgetattr(slave) == original, "Terminal settings were not restored"
            assert secret not in captured, "Credential appeared in terminal output"
            result = "PASS: provider saved, routes created/edited/reordered/deleted, default cleared, key masked, mode 0600, terminal restored\n"
            (run / "output.log").write_text(result)
            print(result.strip())
            print(f"Run: {run.relative_to(root)}")
    except Exception as error:
        # Do not persist terminal captures or config because they could contain keys.
        (run / "output.log").write_text(f"FAIL: {type(error).__name__}: {error}\n")
        raise
    finally:
        if process is not None and process.poll() is None:
            process.kill()
            process.wait(timeout=2)
        termios.tcsetattr(slave, termios.TCSANOW, original)
        os.close(master)
        os.close(slave)


if __name__ == "__main__":
    main()

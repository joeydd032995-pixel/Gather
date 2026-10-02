#!/usr/bin/env python3
"""Exercise the installed desktop app on a disposable GitHub Actions runner."""
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request

if os.environ.get("GITHUB_ACTIONS") != "true":
    raise SystemExit("This installer smoke test runs only on disposable GitHub Actions runners.")

ROOT = Path(__file__).resolve().parents[1]
WORK = Path(os.environ["RUNNER_TEMP"]) / "gather-desktop-smoke"
WORK.mkdir(exist_ok=True)
REPORT = WORK / "window.json"
CLOSE = REPORT.with_suffix(".close")
BUNDLES = ROOT / "apps/desktop/src-tauri/target/release/bundle"
ENV = dict(os.environ, GATHER_DESKTOP_SMOKE_REPORT=str(REPORT),
           GATHER_AUTH_MODE="env", GATHER_API_TOKEN="gather-ci-desktop-smoke",
           GATHER_MEMORY_PROFILE="low")
primary = None
data = None
output = None


def wait_for(check, description, timeout=120):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        value = check()
        if value:
            return value
        time.sleep(0.15)
    raise AssertionError("Timed out: " + description)


def healthy():
    try:
        with urllib.request.urlopen("http://127.0.0.1:7601/healthz", timeout=1) as response:
            return response.status == 200
    except (urllib.error.URLError, TimeoutError, ConnectionError):
        return False


def report(phase, pid):
    try:
        value = json.loads(REPORT.read_text())
        if value["phase"] == phase and value["pid"] == pid:
            return value
    except (OSError, ValueError, KeyError):
        pass
    return None


def pids():
    return (int((data / "daemon.pid").read_text().strip()),
            int((data / "pgdata/postmaster.pid").read_text().splitlines()[0]))


def start(exe):
    global primary, data, output
    output = (WORK / ("launch-" + str(time.time_ns()) + ".log")).open("wb")
    primary = subprocess.Popen([str(exe)], env=ENV, stdout=output, stderr=subprocess.STDOUT)
    def ready():
        if primary.poll() is not None:
            raise AssertionError("Desktop exited during startup: " + str(primary.returncode))
        return report("ready", primary.pid)
    state = wait_for(ready, "native window minimized and ready")
    assert state["minimized"], state
    data = Path(state["data_dir"])
    wait_for(healthy, "bundled database and daemon healthy")
    assert (data / "desktop.lock").is_file()
    print("PASS: installed app starts its private database and daemon", flush=True)
    return pids()


def close():
    CLOSE.touch()
    assert primary.wait(timeout=30) == 0
    output.close()
    wait_for(lambda: not healthy(), "owned daemon stops on app exit", 30)
    wait_for(lambda: not (data / "pgdata/postmaster.pid").exists(),
             "owned PostgreSQL stops on app exit", 30)
    assert not (data / "daemon.pid").exists()
    print("PASS: normal exit stops owned services", flush=True)


def installed_exe():
    if sys.platform == "linux":
        package, = BUNDLES.glob("deb/*.deb")
        subprocess.run(["sudo", "dpkg", "-i", str(package)], check=True)
        exe = shutil.which("gather-desktop")
        assert exe, "Installed desktop binary missing"
        return Path(exe)
    if sys.platform == "darwin":
        app, = BUNDLES.glob("macos/*.app")
        target = WORK / app.name
        subprocess.run(["ditto", str(app), str(target)], check=True)
        return target / "Contents/MacOS/gather-desktop"
    installer, = BUNDLES.glob("nsis/*.exe")
    target = WORK / "installed"
    subprocess.run([str(installer), "/S", "/D=" + str(target)], check=True, timeout=180)
    return target / "gather-desktop.exe"


try:
    assert not healthy(), "Runner already has a daemon; do not adopt unrelated services"
    exe = installed_exe()
    assert exe.is_file(), exe
    initial = start(exe)
    lock_identity = (data / "desktop.lock").stat().st_ino
    for attempt in range(3):
        REPORT.unlink(missing_ok=True)
        secondary = subprocess.Popen([str(exe)], env=ENV,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        assert secondary.wait(timeout=20) == 0, "Second launch did not exit successfully"
        state = wait_for(lambda: report("focused", primary.pid),
                         "primary window restored and focused", 20)
        assert state["focused"] and not state["minimized"], state
        wait_for(lambda: not (data / "focus.request").exists(), "focus request consumed", 10)
        assert primary.poll() is None, "Second launch stopped the primary"
        assert healthy() and pids() == initial, "Second launch disrupted shared services"
        assert (data / "desktop.lock").stat().st_ino == lock_identity, "Lock file replaced"
        print("PASS: second launch focuses primary and preserves service PIDs", flush=True)
    if sys.platform == "darwin":
        REPORT.unlink(missing_ok=True)
        REPORT.with_suffix(".minimize").touch()
        wait_for(lambda: report("ready", primary.pid), "window minimized for Dock/Finder reopen")
        subprocess.run(["open", str(exe.parents[2])], check=True, timeout=20)
        state = wait_for(lambda: report("focused", primary.pid), "Launch Services restores focus", 20)
        assert state["focused"] and not state["minimized"], state
        assert pids() == initial and primary.poll() is None
        print("PASS: Finder/Dock reopen restores primary without changing services", flush=True)
    close()
    recovered = start(exe)
    print("PASS: OS instance lock can be acquired after normal exit", flush=True)
    primary.kill()
    primary.wait(timeout=20)
    output.close()
    wait_for(healthy, "services survive an app crash", 10)
    adopted = start(exe)
    assert adopted == recovered, "Crash recovery did not adopt its existing services"
    print("PASS: crash releases instance lock and replacement adopts owned services", flush=True)
    close()
    print("PASS: complete packaged desktop lifecycle smoke on " + sys.platform, flush=True)
finally:
    if primary is not None and primary.poll() is None:
        CLOSE.touch()
        try:
            primary.wait(timeout=30)
        except subprocess.TimeoutExpired:
            primary.kill()
            primary.wait(timeout=20)
    if output is not None and not output.closed:
        output.close()
    if sys.exc_info()[0]:
        for log in list(WORK.glob("launch-*.log")) + (
                list((data / "logs").glob("*.log")) if data else []):
            text = log.read_text(errors="replace")
            text = re.sub(r"postgres://[^\s]+", "postgres://[redacted]", text)
            print("\n".join((["--- " + log.name] + text.splitlines()[-50:])), file=sys.stderr)

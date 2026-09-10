#!/usr/bin/env python3
"""Check actual daemon thread masks, including Tokio and SQLite workers."""
import os
import pathlib
import subprocess
import tempfile
import time
import urllib.request

root = pathlib.Path(__file__).resolve().parent.parent
with tempfile.TemporaryDirectory() as tmp:
    env = dict(os.environ, BAUD_ADDR="127.0.0.1:17749",
               BAUD_DB=f"sqlite://{tmp}/db?mode=rwc", BAUD_SNAPSHOT_STORE=f"{tmp}/snapshots")
    with open(f"{tmp}/server.log", "w+") as log:
        proc = subprocess.Popen([str(root / "target/debug/baud-server")], env=env, stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 20
            while True:
                if proc.poll() is not None:
                    log.seek(0)
                    raise AssertionError(log.read())
                try:
                    urllib.request.urlopen("http://127.0.0.1:17749/health", timeout=1).close()
                    break
                except OSError:
                    assert time.monotonic() < deadline, "server did not become healthy"
                    time.sleep(0.1)
            masks = []
            for task in pathlib.Path(f"/proc/{proc.pid}/task").iterdir():
                try:
                    masks.append(os.sched_getaffinity(int(task.name)))
                except ProcessLookupError:
                    pass
            assert len(masks) >= 3, masks
            assert all(mask == masks[0] for mask in masks), masks
            assert 0 < len(masks[0]) <= 2, masks
            print(f"PASS: {len(masks)} daemon threads share CPUs {sorted(masks[0])}")
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()

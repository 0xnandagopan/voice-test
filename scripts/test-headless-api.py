#!/usr/bin/env python3
"""Run the real-API browser walkthrough against an isolated local test database.
Requires built target/debug/v0-app, web/dist and web/node_modules.
TEST_DATABASE_URL must identify a disposable local database; no provider calls occur.
"""
import os
from pathlib import Path
import secrets
import socket
import subprocess
import tempfile
import time
import urllib.request
from urllib.parse import urlparse

root = Path(__file__).resolve().parents[1]
database = os.environ.get("TEST_DATABASE_URL", "")
parsed = urlparse(database)
if parsed.hostname not in ("127.0.0.1", "localhost", "::1") or not parsed.path.endswith("_e2e"):
    raise SystemExit("TEST_DATABASE_URL must be a loopback database whose name ends in _e2e.")
binary = root / "target/debug/v0-app"
if not binary.exists() or not (root / "web/dist/index.html").exists():
    raise SystemExit("Build the Rust application and web production assets first.")
with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    port = listener.getsockname()[1]
origin = f"http://127.0.0.1:{port}"
password = secrets.token_urlsafe(24)
hashed = subprocess.run([str(binary), "hash-password"], input=password+"\n", text=True, capture_output=True, check=True, cwd=root).stdout.strip()
env = os.environ.copy()
env.update(DATABASE_URL=database, APP_ORIGIN=origin, BIND_ADDR=f"127.0.0.1:{port}",
    COOKIE_SECURE="false", AGENCY_NAME="Synthetic test agency", OPERATOR_USERNAME="headless-test",
    OPERATOR_PASSWORD_HASH=hashed, INVITATION_SIGNING_KEY=secrets.token_hex(32),
    VOICE_AGENT_API_KEY="", VOICE_TEST_ENABLED="false", VOICE_PUBLIC_ORIGIN="", WEB_DIST=str(root / "web/dist"), RUST_LOG="warn",
    TEST_BASE_URL=origin, TEST_OPERATOR_USERNAME="headless-test", TEST_OPERATOR_PASSWORD=password)
subprocess.run([str(binary), "migrate"], env=env, cwd=root, check=True)
with tempfile.TemporaryFile() as log:
    server = subprocess.Popen([str(binary)], env=env, cwd=root, stdout=log, stderr=log)
    try:
        for _ in range(100):
            if server.poll() is not None:
                raise RuntimeError("Test API exited before becoming ready.")
            try:
                with urllib.request.urlopen(origin+"/api/ready", timeout=1) as response:
                    if response.status == 200:
                        break
            except OSError:
                time.sleep(0.1)
        else:
            raise RuntimeError("Test API readiness timed out.")
        subprocess.run(["npm", "run", "test:api"], env=env, cwd=root / "web", check=True)
    finally:
        server.terminate()
        try:
            server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait()

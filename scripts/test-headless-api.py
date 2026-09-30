#!/usr/bin/env python3
"""Run the real-API browser walkthrough against an isolated local test database.
Requires built target/debug/v0-app, web/dist and web/node_modules.
TEST_DATABASE_URL must identify a disposable local database; no provider calls occur.
Set TEST_SPLIT_ORIGIN=true to build and serve React on a separate loopback origin.
"""
import os
from contextlib import ExitStack
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
split = os.environ.get("TEST_SPLIT_ORIGIN") == "true"
frontend_origin = origin
if split:
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        frontend_port = listener.getsockname()[1]
    frontend_origin = f"http://127.0.0.1:{frontend_port}"
password = secrets.token_urlsafe(24)
hashed = subprocess.run([str(binary), "hash-password"], input=password+"\n", text=True, capture_output=True, check=True, cwd=root).stdout.strip()
env = os.environ.copy()
env.update(DATABASE_URL=database, APP_ORIGIN=frontend_origin, BIND_ADDR=f"127.0.0.1:{port}",
    COOKIE_SECURE="false", AGENCY_NAME="Synthetic test agency", OPERATOR_USERNAME="headless-test",
    OPERATOR_PASSWORD_HASH=hashed, INVITATION_SIGNING_KEY=secrets.token_hex(32),
    VOICE_AGENT_API_KEY="", VOICE_TEST_ENABLED="false", VOICE_PUBLIC_ORIGIN="", WEB_DIST=str(root / "web/dist"), RUST_LOG="warn",
    TEST_BASE_URL=frontend_origin, TEST_API_ORIGIN=origin,
    EVIDENCE_STORAGE_BACKEND="local", SERVE_WEB="false" if split else "true",
    TEST_OPERATOR_USERNAME="headless-test", TEST_OPERATOR_PASSWORD=password)
subprocess.run([str(binary), "migrate"], env=env, cwd=root, check=True)
with ExitStack() as stack:
    log = stack.enter_context(tempfile.TemporaryFile())
    env["EVIDENCE_STORAGE_DIR"] = stack.enter_context(tempfile.TemporaryDirectory(prefix="slug-api-evidence-"))
    processes = []
    server = subprocess.Popen([str(binary)], env=env, cwd=root, stdout=log, stderr=log)
    processes.append(server)
    try:
        if split:
            dist = stack.enter_context(tempfile.TemporaryDirectory(prefix="slug-split-web-"))
            build_env = env.copy()
            build_env["VITE_API_ORIGIN"] = origin
            subprocess.run(["npm", "run", "build", "--", "--outDir", dist], env=build_env, cwd=root / "web", check=True, stdout=log, stderr=log)
            frontend = subprocess.Popen(["node", str(root / "web/node_modules/vite/bin/vite.js"), "preview", "--host", "127.0.0.1", "--port", str(frontend_port), "--strictPort", "--outDir", dist], env=build_env, cwd=root / "web", stdout=log, stderr=log)
            processes.append(frontend)
            for _ in range(100):
                if frontend.poll() is not None:
                    raise RuntimeError("Split-origin frontend exited before becoming ready.")
                try:
                    with urllib.request.urlopen(frontend_origin + "/operator", timeout=1) as response:
                        if response.status == 200:
                            break
                except OSError:
                    time.sleep(0.1)
            else:
                raise RuntimeError("Split-origin frontend readiness timed out.")
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
        for process in reversed(processes):
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()

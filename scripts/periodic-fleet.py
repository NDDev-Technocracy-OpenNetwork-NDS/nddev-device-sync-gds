#!/usr/bin/env python3
"""Owned Linux Accounts/Vault/Git fleet; generated metadata is not GitHub acceptance."""
import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
TEST = "catalog_runtime::fleet_acceptance::native_periodic_fleet_member"


def worker(root, executable, output):
    import resource
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    root, output = Path(root), Path(output)
    assert Path(os.environ["XDG_DATA_HOME"]) == root / "data"
    child = None
    def interrupted(_signal, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupted)
    with (output / f"keyring-{root.name}.log").open("xb") as log:
        daemon = subprocess.Popen(
            ["gnome-keyring-daemon", "--foreground", "--components=secrets",
             "--unlock", "--control-directory", str(root / "control")],
            stdin=subprocess.PIPE, stdout=log, stderr=log)
        try:
            daemon.stdin.write(secrets.token_urlsafe(32).encode())
            daemon.stdin.close()
            end = time.monotonic() + 10
            while time.monotonic() < end:
                assert daemon.poll() is None, "Owned Secret Service exited"
                probe = subprocess.run(
                    ["gdbus", "call", "--session", "--dest", "org.freedesktop.DBus",
                     "--object-path", "/org/freedesktop/DBus", "--method",
                     "org.freedesktop.DBus.GetConnectionUnixProcessID", "org.freedesktop.secrets"],
                    capture_output=True, timeout=2)
                if probe.returncode == 0:
                    assert int(re.findall(rb"\d+", probe.stdout)[-1]) == daemon.pid
                    break
                time.sleep(.1)
            else:
                raise RuntimeError("Owned Secret Service did not initialize")
            for epoch in (0, 1):
                with (output / f"client-{root.name}-{epoch}.log").open("xb") as log:
                    child = subprocess.Popen(
                        [executable, "--exact", TEST, "--ignored", "--nocapture"],
                        env=os.environ | {"NDS_FLEET_EPOCH": str(epoch)},
                        stdout=log, stderr=subprocess.STDOUT)
                    status = child.wait(timeout=3500)
                child = None
                if status:
                    raise RuntimeError(f"Owned Rust member failed: {root.name}, epoch {epoch}")
        finally:
            for process in (child, daemon):
                if process is not None and process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()


def main():
    def interrupted(_signal, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--clients", type=int, choices=range(2, 33), default=32, metavar="2..32")
    parser.add_argument("--repositories", type=int, choices=range(2, 1025), default=1024, metavar="2..1024")
    args = parser.parse_args()
    assert sys.platform == "linux"
    assert args.fixture.is_absolute() and args.fixture.is_file()
    assert args.output.is_absolute() and not args.output.exists()
    assert not args.output.resolve().is_relative_to(ROOT)
    receipt = json.loads(args.fixture.read_text())
    assert receipt["status"] == "running" and receipt["owner_email"].endswith(".invalid")
    for name in ("control_file", "stop_marker"):
        assert Path(receipt[name]).parent.resolve() == args.fixture.parent.resolve()
    try:
        qualify(args, receipt)
    finally:
        Path(receipt["stop_marker"]).touch()


def qualify(args, receipt):
    args.output.mkdir(mode=0o700, parents=True)
    source = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    dirty = bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT))
    full = args.clients == 32 and args.repositories == 1024
    assert not (full and dirty), "Full qualification requires committed source"
    assert shutil.disk_usage(args.output).free >= (16 if full else 4) * 1024**3
    with (args.output / "build.log").open("x") as log:
        built = subprocess.run(
            ["cargo", "test", "--locked", "--features", "catalog-runtime,commit-sync",
             "--lib", "--no-run", "--message-format=json"], cwd=ROOT,
            env=os.environ | {"CARGO_PROFILE_TEST_OPT_LEVEL": "3"},
            stdout=subprocess.PIPE, stderr=log, text=True, check=True, timeout=600)
    artifacts = [json.loads(line) for line in built.stdout.splitlines() if line.startswith("{")]
    executables = [x["executable"] for x in artifacts if x.get("reason") == "compiler-artifact"
                   and x.get("executable") and x.get("profile", {}).get("test")]
    assert len(executables) == 1
    executable = executables[0]
    deadline = time.monotonic() + min(3500, receipt["expires_at_unix"] - time.time() - 10)
    if full:
        assert deadline - time.monotonic() >= 3300, "Start a fresh fixture after precompiling"
    processes, connections, history, identities = [], {}, [], {}

    def check():
        if time.monotonic() > deadline:
            raise TimeoutError("Owned fleet deadline")
        for process in processes:
            if process.poll() is not None and process.returncode != 0:
                raise RuntimeError("Owned fleet supervisor failed; inspect private logs")
        sizes = [p.stat().st_size for p in args.output.glob("*.log")]
        assert all(size <= 128 * 1024**2 for size in sizes) and sum(sizes) <= 2 * 1024**3

    def event(phase, **facts):
        value = {"phase": phase, "at_ms": time.time_ns() // 1_000_000, **facts}
        history.append(value)
        (args.output / "progress.json").write_text(json.dumps(value) + "\n")
        print(json.dumps(value), flush=True)

    def read(connection):
        data = bytearray()
        while not data.endswith(b"\n"):
            check()
            try:
                chunk = connection.recv(1)
            except socket.timeout:
                continue
            if not chunk:
                raise RuntimeError("Owned fleet control channel closed")
            data.extend(chunk)
            assert len(data) <= 65536
        return json.loads(data)

    def send(index, action, **facts):
        connections[index].sendall(json.dumps({"action": action, **facts}).encode() + b"\n")

    def all_clients(action, **facts):
        for index in range(args.clients):
            send(index, action, **facts)
        result = [read(connections[index]) for index in range(args.clients)]
        event(action, clients=args.clients)
        return result

    def mode(value):
        Path(receipt["control_file"]).write_text(json.dumps({"mode": value}))
        end = time.monotonic() + 15
        while time.monotonic() < end:
            check()
            current = json.loads(args.fixture.read_text())["status"]
            if current == ("running" if value == "online" else "offline"):
                return
            time.sleep(.1)
        raise TimeoutError("Owned NDS fixture did not change mode")

    class Metadata(http.server.BaseHTTPRequestHandler):
        def setup(self):
            self.request.settimeout(5)
            super().setup()

        def do_GET(self):
            match = re.fullmatch(r"/repos/generated-owner/repository-(\d{4})", self.path)
            if not match or not 1 <= int(match[1]) <= args.repositories:
                self.send_error(404)
                return
            if self.headers.get("Authorization") != "Bearer intentionally-invalid-github-fleet-value":
                self.send_error(401)
                return
            data = json.dumps({"id": int(match[1]), "name": f"repository-{int(match[1]):04}",
                               "full_name": f"generated-owner/repository-{int(match[1]):04}",
                               "visibility": "private",
                               "owner": {"id": 1, "login": "generated-owner", "type": "Organization"},
                               "private": True, "archived": False, "default_branch": "main"}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def log_message(self, *_args):
            pass

    class MetadataServer(http.server.ThreadingHTTPServer):
        request_queue_size = 32
        daemon_threads = False

    provider = MetadataServer(("127.0.0.1", 0), Metadata)
    thread = threading.Thread(target=provider.serve_forever, daemon=True)
    thread.start()
    scratch = Path.home() / ".local/state"
    scratch.mkdir(parents=True, exist_ok=True)
    assert shutil.disk_usage(scratch).free >= (16 if full else 4) * 1024**3
    with tempfile.TemporaryDirectory(prefix="nds-git-fleet-", dir=scratch) as directory:
        base = Path(directory)
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(str(base / "control.sock"))
        os.chmod(base / "control.sock", 0o600)
        listener.listen(64)
        listener.settimeout(1)
        try:
            remotes, seed = base / "remotes", base / "seed"
            remotes.mkdir()
            seed.mkdir()
            def git(path, *args):
                return subprocess.check_output(
                    ["git", "-c", "commit.gpgsign=false", "-c", "user.name=NDS generated fleet",
                     "-c", "user.email=fleet@example.invalid", "-c", "core.hooksPath=/dev/null", *args], cwd=path, text=True,
                    env=os.environ | {"GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null",
                                      "GIT_TERMINAL_PROMPT": "0"},
                    stderr=subprocess.DEVNULL, timeout=15).strip()
            git(seed, "init", "-q", "-b", "main")
            (seed / "initial.txt").write_text("generated initial content\n")
            git(seed, "add", "initial.txt")
            git(seed, "commit", "-qm", "test(gds): generated fleet seed")
            for identity in range(1, args.repositories + 1):
                check()
                git(base, "clone", "--bare", "--shared", str(seed),
                    str(remotes / f"repository-{identity:04}.git"))
            event("remotes_ready", repositories=args.repositories)

            def start(index):
                root = base / str(index)
                for name in ("data/keyrings", "state", "config", "cache", "runtime", "control"):
                    (root / name).mkdir(parents=True, mode=0o700)
                env = {k: v for k, v in os.environ.items()
                       if k in ("PATH", "HOME", "LANG", "LC_ALL", "LD_LIBRARY_PATH")}
                env.update({k: str(root / v) for k, v in (
                    ("XDG_DATA_HOME", "data"), ("XDG_STATE_HOME", "state"),
                    ("XDG_CONFIG_HOME", "config"), ("XDG_CACHE_HOME", "cache"),
                    ("XDG_RUNTIME_DIR", "runtime"))})
                env.update({
                    "NDS_GIT_FLEET_ROOT": str(root), "NDS_FLEET_INDEX": str(index),
                    "NDS_FLEET_COUNT": str(args.clients),
                    "NDS_GIT_FLEET_REPOSITORIES": str(args.repositories),
                    "NDS_GIT_FLEET_PROVIDER": f"http://127.0.0.1:{provider.server_port}",
                    "NDS_GIT_FLEET_REMOTES": str(remotes),
                    "NDS_FLEET_SOCKET": str(base / "control.sock"),
                    "NDS_FLEET_TOKEN_SOCKET": str(base / "session.sock"),
                    "NDS_FIXTURE_RECEIPT": str(args.fixture),
                    "NDS_FLEET_SOURCE": "unknown" if dirty else source})
                with (args.output / f"supervisor-{index}.log").open("xb") as log:
                    processes.append(subprocess.Popen(
                        ["dbus-run-session", "--", sys.executable, __file__, "--worker",
                         str(root), executable, str(args.output)],
                        env=env, stdout=log, stderr=log, start_new_session=True))

            def enroll(batch, epoch):
                pending = set(batch)
                while pending:
                    check()
                    try:
                        connection, _ = listener.accept()
                    except socket.timeout:
                        continue
                    connection.settimeout(1)
                    hello = read(connection)
                    index = hello["index"]
                    assert index in pending and hello["epoch"] == epoch
                    connections[index] = connection
                    send(index, "initialize")
                    pending.remove(index)
                for index in batch:
                    current = read(connections[index])
                    if epoch:
                        assert current == identities[index]
                    else:
                        identities[index] = current
                    event("restored" if epoch else "enrolled", device=index)

            start(0)
            enroll([0], 0)
            for begin in range(1, args.clients, 8):
                batch = list(range(begin, min(begin + 8, args.clients)))
                for index in batch:
                    start(index)
                enroll(batch, 0)
            assert len({x["device"] for x in identities.values()}) == args.clients
            assert len({(x["user"], x["tenant"]) for x in identities.values()}) == 1
            send(0, "seed")
            assert read(connections[0])["seeded"] == args.repositories
            all_clients("connect")
            connection_rows = []
            for index in range(args.clients):
                files = list((base / str(index) / "replica").rglob("*.db"))
                assert len(files) == 1
                with sqlite3.connect(files[0].as_uri() + "?mode=ro", uri=True) as db:
                    rows = db.execute("SELECT count(*) FROM v3_entities WHERE kind='repository_state' AND id LIKE 'nds:connection:%'").fetchone()[0]
                assert rows == args.repositories, f"Unexpected connection-intent count: {rows}"
                connection_rows.append(rows)
            all_clients("edit")
            def dirty_witnesses():
                witnesses = []
                for index in range(args.clients):
                    path = base / str(index) / "Developer/NDS/generated-owner/repository-0001"
                    witnesses.append({"head": git(path, "rev-parse", "HEAD"),
                                      "branch": (path / ".git/HEAD").read_text(),
                                      "index_sha256": hashlib.sha256((path / ".git/index").read_bytes()).hexdigest(),
                                      "dirty_sha256": hashlib.sha256((path / "initial.txt").read_bytes()).hexdigest()})
                return witnesses
            dirty_before = dirty_witnesses()
            send(0, "commit")
            head = read(connections[0])["head"]
            boundary = time.time_ns() // 1_000_000
            send(0, "start")
            assert read(connections[0])["started"]
            send(0, "published", head=head)
            assert read(connections[0])["published"] == head
            for index in range(1, args.clients):
                send(index, "start")
            for index in range(1, args.clients):
                assert read(connections[index])["started"]
            canaries = all_clients("canary")
            assert all(value["acknowledged"] for value in canaries)
            all_clients("wait", after_ms=boundary, head=head)
            assert dirty_witnesses() == dirty_before
            # A second complete observation uses the production 300-second round
            # delay; no queue clock, retry or per-device allowance is changed.
            boundary = time.time_ns() // 1_000_000
            all_clients("wait", after_ms=boundary, head=head)
            assert dirty_witnesses() == dirty_before
            event("periodic_round_qualified", repositories=args.repositories)
            mode("offline")
            all_clients("offline")

            def originals(index):
                files = list((base / str(index) / "replica").rglob("*.db"))
                assert len(files) == 1
                with sqlite3.connect(files[0].as_uri() + "?mode=ro", uri=True) as db:
                    values = db.execute("SELECT id,body FROM v3_outbox").fetchall()
                assert len(values) == 1
                return {key: hashlib.sha256(body).hexdigest() for key, body in values}
            before = [originals(index) for index in range(args.clients)]
            all_clients("stop")
            for connection in connections.values():
                connection.close()
            connections.clear()
            enroll(list(range(args.clients)), 1)
            assert before == [originals(index) for index in range(args.clients)]
            event("cold_offline_originals_preserved", clients=args.clients)
            mode("online")
            all_clients("drain")
            send(args.clients - 1, "commit")
            head = read(connections[args.clients - 1])["head"]
            boundary = time.time_ns() // 1_000_000
            publisher = args.clients - 1
            send(publisher, "start")
            assert read(connections[publisher])["started"]
            send(publisher, "published", head=head)
            assert read(connections[publisher])["published"] == head
            for index in range(args.clients):
                if index != publisher:
                    send(index, "start")
            for index in range(args.clients):
                if index != publisher:
                    assert read(connections[index])["started"]
            all_clients("wait", after_ms=boundary, head=head)
            assert dirty_witnesses() == dirty_before
            all_clients("stop")
            for process in processes:
                assert process.wait(timeout=15) == 0
            proof = {
                "source": source, "dirty_source": dirty, "clients": args.clients,
                "repositories": args.repositories, "distinct_devices": len(identities),
                "scope": "Actual elected Rust GDS/Vault lifetimes, isolated native credentials, "
                         "genuine session, SQLite, NDS HTTP/PostgreSQL and Git. Generated loopback "
                         "repository metadata is not GitHub discovery or provider rate-limit acceptance.",
                "phases": history, "last_existing_commit": head,
                "connection_rows_per_replica": connection_rows,
                "concurrent_vault_canaries": canaries,
                "dirty_witnesses": dirty_before,
                "encrypted_pending_originals_preserved": True}
        finally:
            for connection in connections.values():
                connection.close()
            listener.close()
            for process in processes:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.wait()
            provider.shutdown()
            provider.server_close()
            thread.join(timeout=5)
            assert not thread.is_alive()
    assert not base.exists() and all(process.poll() == 0 for process in processes)
    proof["cleanup"] = {"native_supervisors_exited": True, "owned_state_removed": True,
                        "metadata_server_stopped": True}
    (args.output / "receipt.json").write_text(json.dumps(proof, indent=2) + "\n")
    event("complete", clients=args.clients, repositories=args.repositories)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--worker":
        worker(*sys.argv[2:])
    else:
        main()

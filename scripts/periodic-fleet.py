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
FEATURES = "catalog-runtime,commit-sync"


def source_identity():
    def git(*args):
        return subprocess.check_output(["git", *args], cwd=ROOT, text=True, timeout=15).strip()
    def digest(name):
        return hashlib.sha256((ROOT / name).read_bytes()).hexdigest()
    return {"commit": git("rev-parse", "HEAD"), "tree": git("rev-parse", "HEAD^{tree}"),
            "dirty": bool(git("status", "--porcelain", "--untracked-files=all")),
            "lock_sha256": digest("Cargo.lock"),
            "toolchain_sha256": digest("rust-toolchain.toml")}


def build_member(output):
    with (output / "build.log").open("x") as log:
        built = subprocess.run(
            ["cargo", "test", "--locked", "--features", FEATURES,
             "--lib", "--no-run", "--message-format=json"], cwd=ROOT,
            env=os.environ | {"CARGO_PROFILE_TEST_OPT_LEVEL": "3"},
            stdout=subprocess.PIPE, stderr=log, text=True, check=True, timeout=600)
    artifacts = [json.loads(line) for line in built.stdout.splitlines() if line.startswith("{")]
    executables = [x["executable"] for x in artifacts if x.get("reason") == "compiler-artifact"
                   and x.get("executable") and x.get("profile", {}).get("test")
                   and x.get("target", {}).get("name") == "nddev_device_sync_gds"
                   and x["target"].get("kind") == ["lib"]]
    if len(executables) != 1:
        raise ValueError("Expected exactly one owning GDS library test executable")
    return Path(executables[0])


def stage_member(source, output, expected=None):
    staged = output / "native-fleet-member"
    # Hash the copied bytes, rather than trusting a mutable Cargo cache path.
    with source.open("rb") as binary, staged.open("xb") as target:
        remaining = os.fstat(binary.fileno()).st_size
        if not 0 < remaining <= 1024**3:
            raise ValueError("Fleet member size is outside its bound")
        while remaining:
            chunk = binary.read(min(remaining, 1024 * 1024))
            if not chunk:
                raise ValueError("Fleet member changed while copying")
            target.write(chunk)
            remaining -= len(chunk)
        if binary.read(1):
            raise ValueError("Fleet member changed while copying")
    with staged.open("rb") as binary:
        digest = hashlib.file_digest(binary, "sha256").hexdigest()
    if expected is not None and digest != expected:
        raise ValueError("Fleet member bytes differ from the selected manifest")
    staged.chmod(0o500)
    return staged, digest


def member_manifest(path, identity):
    if path.stat().st_size > 65536:
        raise ValueError("Fleet member manifest exceeds its bound")
    value = json.loads(path.read_text())
    if (value.get("schema") != 1 or value.get("source") != identity or identity["dirty"]
            or value.get("features") != FEATURES or value.get("test") != TEST
            or value.get("executable") != "native-fleet-member"
            or not re.fullmatch(r"[0-9a-f]{64}", value.get("executable_sha256", ""))):
        raise ValueError("Fleet member manifest differs from the clean selected source/profile")
    return value


def prepare_member():
    parser = argparse.ArgumentParser(description="Freeze the owning Linux fleet test artifact; starts no application")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args(sys.argv[2:])
    if (sys.platform != "linux" or not args.output.is_absolute() or args.output.exists()
            or args.output.resolve().is_relative_to(ROOT)):
        parser.error("requires a new absolute Linux artifact directory outside source")
    identity = source_identity()
    if identity["dirty"]:
        parser.error("artifact preparation requires clean committed source")
    args.output.mkdir(mode=0o700, parents=True)
    executable, digest = stage_member(build_member(args.output), args.output)
    if source_identity() != identity:
        raise ValueError("Source changed while preparing the fleet member")
    value = {"schema": 1, "source": identity, "features": FEATURES, "test": TEST,
             "executable": executable.name, "executable_sha256": digest,
             "rustc": subprocess.check_output(["rustc", "-vV"], cwd=ROOT, text=True, timeout=15)}
    with (args.output / "member.json").open("x") as manifest:
        json.dump(value, manifest, indent=2)
        manifest.write("\n")
    print("Fleet member frozen; source/profile and bytes must be verified by the native consumer", flush=True)


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
                    status = child.wait(timeout=14300)
                child = None
                if status:
                    raise RuntimeError(f"Owned Rust member failed: {root.name}, epoch {epoch}")
        finally:
            for process in (child, daemon):
                if process is not None and process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=45 if process is child else 5)
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
    parser.add_argument("--work-root", type=Path, required=True,
                        help="Existing admitted native fixture directory outside the source tree; no HOME fallback")
    parser.add_argument("--member-manifest", type=Path,
                        help="Previously frozen member.json for this exact clean source; skips Cargo on the app host")
    parser.add_argument("--clients", type=int, choices=range(2, 33), default=32, metavar="2..32")
    parser.add_argument("--repositories", type=int, choices=range(2, 1025), default=1024, metavar="2..1024")
    parser.add_argument("--guard-contention", action="store_true", help="Also exercise real SQLite admission pressure against the original proof")
    args = parser.parse_args()
    assert sys.platform == "linux"
    assert args.fixture.is_absolute() and args.fixture.is_file()
    assert args.output.is_absolute() and not args.output.exists()
    assert not args.output.resolve().is_relative_to(ROOT)
    assert args.work_root.is_absolute() and args.work_root.is_dir()
    args.work_root = args.work_root.resolve(strict=True)
    assert not args.work_root.is_relative_to(ROOT)
    # Refuse stale source/profile before reading or stopping the owned fixture.
    args.identity = source_identity()
    args.member = None
    if args.member_manifest is not None:
        if not args.member_manifest.is_absolute():
            parser.error("member manifest must be an explicit absolute path")
        args.member = member_manifest(args.member_manifest, args.identity)
    args.staged, args.executable_sha256 = prepare_execution(args)
    receipt = json.loads(args.fixture.read_text())
    assert receipt["status"] == "running" and receipt["owner_email"].endswith(".invalid")
    for name in ("control_file", "stop_marker"):
        assert Path(receipt[name]).parent.resolve() == args.fixture.parent.resolve()
    try:
        qualify(args, receipt)
    finally:
        Path(receipt["stop_marker"]).touch()


def prepare_execution(args):
    """Resolve and check bytes before acquiring the Server fixture lifetime."""
    args.output.mkdir(mode=0o700, parents=True)
    dirty = args.identity["dirty"]
    full = args.clients == 32 and args.repositories == 1024
    assert not (full and dirty), "Full qualification requires committed source"
    assert shutil.disk_usage(args.output).free >= (16 if full else 4) * 1024**3
    # Both process epochs must execute the same qualified bytes, independently
    # of subsequent Cargo builds or managed-cache retirement.
    if args.member is None:
        staged, executable_sha256 = stage_member(build_member(args.output), args.output)
    else:
        staged, executable_sha256 = stage_member(
            args.member_manifest.parent / args.member["executable"], args.output,
            args.member["executable_sha256"])
    if source_identity() != args.identity:
        raise ValueError("Source changed before fleet startup")
    listed = subprocess.run([str(staged), "--exact", TEST, "--ignored", "--list"],
                            capture_output=True, text=True, check=True, timeout=15)
    if listed.stdout.splitlines() != [f"{TEST}: test", "", "1 test, 0 benchmarks"]:
        raise ValueError("Selected executable does not contain the exact owning fleet test")
    return staged, executable_sha256


def qualify(args, receipt):
    source, dirty = args.identity["commit"], args.identity["dirty"]
    full = args.clients == 32 and args.repositories == 1024
    staged, executable_sha256 = args.staged, args.executable_sha256
    executable = str(staged)
    deadline = time.monotonic() + min(14300, receipt["expires_at_unix"] - time.time() - 10)
    if full:
        assert deadline - time.monotonic() >= 14000, "Start a fresh 14400-second fixture after precompiling"
    processes, connections, history, identities = [], {}, [], {}
    rounds = []

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
        result = []
        for index in range(args.clients):
            result.append(read(connections[index]))
            if action == "wait":
                # Raw replies are diagnostic, not a qualified round or fleet.
                # Keep received members if a later control channel closes.
                partial = args.output / "round-replies.pending"
                partial.write_text(json.dumps({"round": len(rounds) + 1,
                    "received_clients": len(result), "total_clients": args.clients,
                    "fleet_qualified": False, "member_replies": result}) + "\n")
                partial.chmod(0o600)
                partial.replace(args.output / "round-replies.json")
        event(action, clients=args.clients)
        return result

    def qualify_round(boundary, head):
        results = all_clients("wait", after_ms=boundary, head=head)
        for result in results:
            assert result["observed"] == args.repositories
            assert result["head"] == head and result["dirty_preserved"]
            assert result["dirty_refusal_observed"]
            successful = result["successful_repository_ids"]
            assert len(set(successful)) == len(successful)
            assert set(successful) <= set(range(2, args.repositories + 1))
            assert 2 in successful
            assert sum(result["terminal_outcomes"].values()) == args.repositories
        rounds.append(results)
        (args.output / "rounds.json").write_text(json.dumps(rounds, indent=2) + "\n")

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
    # Placement is supplied by the native execution/storage owner. A missing
    # work root never falls back to HOME or creates another managed root.
    scratch = args.work_root
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
                for name in ("data/keyrings", "state", "config", "cache", "runtime", "control", "tmp"):
                    (root / name).mkdir(parents=True, mode=0o700)
                env = {k: v for k, v in os.environ.items()
                       if k in ("PATH", "HOME", "LANG", "LC_ALL", "LD_LIBRARY_PATH")}
                env.update({k: str(root / v) for k, v in (
                    ("XDG_DATA_HOME", "data"), ("XDG_STATE_HOME", "state"),
                    ("XDG_CONFIG_HOME", "config"), ("XDG_CACHE_HOME", "cache"),
                    ("XDG_RUNTIME_DIR", "runtime"), ("TMPDIR", "tmp"))})
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
            if args.guard_contention:
                files = list((base / "0/replica").rglob("*.db"))
                assert len(files) == 1
                send(0, "guard-contention", database=str(files[0]))
                facts = read(connections[0])
                (args.output / "guard-contention.json").write_text(json.dumps(facts, indent=2) + "\n")
                assert facts["passed"], "Original proof/cursor was lost during real native admission pressure"
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
            qualify_round(boundary, head)
            assert dirty_witnesses() == dirty_before
            # A second complete observation uses the production 300-second round
            # delay; no queue clock, retry or per-device allowance is changed.
            boundary = time.time_ns() // 1_000_000
            qualify_round(boundary, head)
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
            qualify_round(boundary, head)
            assert dirty_witnesses() == dirty_before
            successful = [set().union(*(set(values[index]["successful_repository_ids"])
                                        for values in rounds)) for index in range(args.clients)]
            assert all(ids == set(range(2, args.repositories + 1)) for ids in successful), \
                "A clean fixture repository never produced a successful in-scope completion"
            round_outcomes = []
            for values in rounds:
                outcomes = {}
                for value in values:
                    for outcome, count in value["terminal_outcomes"].items():
                        outcomes[outcome] = outcomes.get(outcome, 0) + count
                round_outcomes.append(outcomes)
            all_clients("stop")
            for process in processes:
                assert process.wait(timeout=15) == 0
            with staged.open("rb") as binary:
                assert hashlib.file_digest(binary, "sha256").hexdigest() == executable_sha256
            proof = {
                "source": source, "dirty_source": dirty, "clients": args.clients,
                "fixture_work_root": str(scratch),
                "native_executable": staged.name,
                "native_executable_sha256": executable_sha256,
                "member_prebuilt": args.member is not None,
                "source_identity": args.identity,
                "repositories": args.repositories, "distinct_devices": len(identities),
                "scope": "Actual elected Rust GDS/Vault lifetimes, isolated native credentials, "
                         "genuine session, SQLite, NDS HTTP/PostgreSQL and Git. Generated loopback "
                         "repository metadata is not GitHub discovery or provider rate-limit acceptance.",
                "phases": history, "last_existing_commit": head,
                "connection_rows_per_replica": connection_rows,
                "concurrent_vault_canaries": canaries,
                "round_terminal_outcomes": round_outcomes,
                "successful_repositories_per_device": [len(ids) for ids in successful],
                "dirty_witnesses": dirty_before,
                "encrypted_pending_originals_preserved": True}
        finally:
            for connection in connections.values():
                connection.close()
            listener.close()
            for process in processes:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
            cleanup_deadline = time.monotonic() + 60
            for process in processes:
                if process.poll() is None:
                    try:
                        process.wait(timeout=max(.1, cleanup_deadline - time.monotonic()))
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
    elif len(sys.argv) > 1 and sys.argv[1] == "prepare-member":
        prepare_member()
    else:
        main()

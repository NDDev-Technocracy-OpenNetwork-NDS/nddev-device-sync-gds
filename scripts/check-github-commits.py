#!/usr/bin/env python3
"""Owned GitHub branch fixture; never reads gh credentials or personal tokens."""
import json
import base64
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request
import uuid

REPOSITORY = "NDDev-Technocracy-OpenNetwork-NDS/nddev-device-sync-github"
ROOT = Path(__file__).resolve().parents[1]


def context(environment):
    if (environment.get("GITHUB_ACTIONS") != "true"
            or environment.get("RUNNER_ENVIRONMENT") != "github-hosted"
            or environment.get("GITHUB_REPOSITORY") != REPOSITORY
            or environment.get("GITHUB_REF") != "refs/heads/main"):
        raise ValueError("Requires the owning repository's trusted main workflow")
    run = environment.get("GITHUB_RUN_ID", "") + "-" + environment.get("GITHUB_RUN_ATTEMPT", "")
    sha = environment.get("GITHUB_SHA", "")
    if not re.fullmatch(r"[0-9]{1,20}-[0-9]{1,6}", run) or not re.fullmatch(r"[0-9a-f]{40}", sha):
        raise ValueError("Invalid immutable workflow identity")
    return run, sha


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *_args, **_kwargs):
        return None


def api(method, route, token, body=None, expected=200):
    request = urllib.request.Request(
        "https://api.github.com/repos/" + REPOSITORY + route,
        data=None if body is None else json.dumps(body).encode(), method=method,
        headers={"Authorization": "Bearer " + token, "Accept": "application/vnd.github+json",
                 "Content-Type": "application/json", "User-Agent": "NDS-owned-Git-fixture",
                 "X-GitHub-Api-Version": "2026-03-10"})
    opener = urllib.request.build_opener(NoRedirect(), urllib.request.ProxyHandler({}))
    try:
        with opener.open(request, timeout=30) as response:
            if response.status != expected:
                raise RuntimeError("Unexpected GitHub fixture status")
            content = response.read(65537)
            if len(content) > 65536:
                raise RuntimeError("GitHub fixture output limit")
            return json.loads(content) if content else None
    except urllib.error.HTTPError as error:
        if method == "GET" and error.code == 404:
            return None
        raise RuntimeError(f"GitHub fixture HTTP {error.code}") from None


def cleanup(receipt, run, sha, token):
    if not receipt.exists():
        return
    value = json.loads(receipt.read_text())
    if (value["repository"] != REPOSITORY or value["run"] != run or value["base"] != sha
            or not re.fullmatch(r"nds-acceptance/" + re.escape(run) + r"-[0-9a-f]{32}", value["branch"])):
        raise ValueError("Refuse cleanup of another fixture")
    if not value["created"]:
        raise RuntimeError("Branch creation outcome unknown; preserve it for inspection")
    metadata = api("GET", "", token)
    if (metadata is None or metadata["id"] != value["repository_id"]
            or metadata["owner"]["id"] != value["owner_id"]):
        raise RuntimeError("Repository identity changed; preserve the branch")
    allowed = value["allowed_heads"]
    if not 1 <= len(allowed) <= 3 or any(not re.fullmatch(r"[0-9a-f]{40}", head) for head in allowed):
        raise ValueError("Invalid fixture commit witnesses")
    route = "/git/ref/heads/" + value["branch"]
    observed = api("GET", route, token)
    if observed is not None:
        if observed["object"]["sha"] not in allowed:
            raise RuntimeError("Unrecognized branch writer; preserve its work")
        # REST DeleteRef has no expected-OID precondition. Use Git's atomic
        # compare-and-delete for this one owned fixture ref instead. A racing
        # writer cannot lose its commit between the inspection and deletion.
        ref = "refs/heads/" + value["branch"]
        environment = {"PATH": "/usr/bin:/bin", "GIT_CONFIG_NOSYSTEM": "1",
                       "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_TERMINAL_PROMPT": "0"}
        with tempfile.TemporaryDirectory(prefix="nds-owned-ref-", dir=receipt.parent) as directory:
            subprocess.run(["/usr/bin/git", "init", "--bare", "--template=", directory],
                           env=environment, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                           check=True, timeout=10)
            authenticated = environment | {"NDS_GITHUB_AUTHORIZATION": "Authorization: Basic "
                + base64.b64encode(("x-access-token:" + token).encode()).decode()}
            result = subprocess.run([
                "/usr/bin/git", "-c", "core.hooksPath=/dev/null", "-c", "credential.helper=",
                "-c", "http.followRedirects=false", "-c", "protocol.allow=never",
                "-c", "protocol.https.allow=always",
                "--config-env=http.https://github.com/.extraHeader=NDS_GITHUB_AUTHORIZATION",
                "push", "--porcelain", "--force-with-lease=" + ref + ":" + observed["object"]["sha"],
                "https://github.com/" + REPOSITORY + ".git", ":" + ref], cwd=directory,
                env=authenticated, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)
            if result.returncode:
                raise RuntimeError("Conditional fixture ref deletion refused; inspect the retained branch")
        if api("GET", route, token) is not None:
            raise RuntimeError("Branch deletion not confirmed")
    value["cleanup"] = "confirmed absent"
    receipt.write_text(json.dumps(value) + "\n")
    print("Owned GitHub fixture branch confirmed absent", flush=True)


def main():
    if len(sys.argv) != 3 or sys.argv[1] not in ("run", "cleanup"):
        raise ValueError("Usage: check-github-commits.py run|cleanup BUILD_JSONL")
    run, sha = context(os.environ)
    if subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip() != sha:
        raise ValueError("Checkout differs from the selected workflow source")
    receipt = Path(os.environ["RUNNER_TEMP"]) / "nds-github-github-acceptance.json"
    token = os.environ["NDS_GITHUB_ACCEPTANCE_TOKEN"]
    if sys.argv[1] == "cleanup":
        cleanup(receipt, run, sha, token)
        return
    binaries = [Path(value["executable"]) for line in Path(sys.argv[2]).read_text().splitlines()
                if (value := json.loads(line)).get("reason") == "compiler-artifact"
                and value.get("target", {}).get("name") == "github_commits" and value.get("executable")]
    if len(binaries) != 1 or not binaries[0].is_file() or not binaries[0].is_relative_to(ROOT / "target"):
        raise ValueError("Expected one locally compiled acceptance executable")
    metadata = api("GET", "", token)
    if (metadata is None or metadata["id"] != int(os.environ["NDS_GITHUB_REPOSITORY_ID"])
            or metadata["owner"]["id"] != int(os.environ["NDS_GITHUB_OWNER_ID"]) or metadata["private"]):
        raise ValueError("Unexpected repository identity")
    branch = "nds-acceptance/" + run + "-" + uuid.uuid4().hex
    value = {"repository": REPOSITORY, "repository_id": metadata["id"], "owner_id": metadata["owner"]["id"],
             "run": run, "branch": branch, "base": sha,
             "allowed_heads": [sha], "created": False}
    with receipt.open("x") as output:
        json.dump(value, output)
    try:
        reply = api("POST", "/git/refs", token, {"ref": "refs/heads/" + branch, "sha": sha}, 201)
        if reply["ref"] != "refs/heads/" + branch or reply["object"]["sha"] != sha:
            raise RuntimeError("Unexpected created branch witness")
        value["created"] = True
        receipt.write_text(json.dumps(value) + "\n")
        environment = os.environ.copy()
        environment["NDS_GITHUB_ACCEPTANCE_RECEIPT"] = str(receipt)
        # This directly executes the built test. No compiler/build script gets
        # the job's write credential during this step.
        subprocess.run([str(binaries[0]), "--ignored", "--exact", "github_existing_commits"],
                       cwd=ROOT, env=environment, check=True, timeout=600)
    finally:
        cleanup(receipt, run, sha, token)


if __name__ == "__main__":
    try:
        main()
    except Exception as failure:
        # Provider response bodies and authorization headers never enter logs.
        print("GitHub fixture failed: " + str(failure), file=sys.stderr)
        raise SystemExit(1) from None

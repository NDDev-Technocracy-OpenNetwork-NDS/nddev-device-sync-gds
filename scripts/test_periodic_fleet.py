"""Fleet artifact/source refusal checks; these do not execute an app or fleet."""
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("fleet", Path(__file__).with_name("periodic-fleet.py"))
fleet = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fleet)


class MemberArtifact(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.real_run = subprocess.run
        self.source = self.root / "source"
        self.source.mkdir()
        (self.source / "Cargo.lock").write_text("generated lock\n")
        (self.source / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "1.99.1"\n')
        for args in [("init", "-q"), ("add", "."), ("commit", "-qm", "generated fixture")]:
            subprocess.run(["git", "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null",
                            "-c", "user.name=NDS generated fixture", "-c", "user.email=fleet@example.invalid",
                            *args], cwd=self.source, check=True, capture_output=True)
        self.addCleanup(patch.stopall)
        patch.object(fleet, "ROOT", self.source).start()
        self.identity = fleet.source_identity()
        self.artifact = self.root / "artifact"
        self.artifact.mkdir()
        self.binary = self.artifact / "native-fleet-member"
        self.binary.write_bytes(b"generated nonexecutable member bytes\n")
        self.manifest = self.artifact / "member.json"
        self.value = {"schema": 1, "source": self.identity, "features": fleet.FEATURES,
                      "test": fleet.TEST, "executable": self.binary.name,
                      "executable_sha256": hashlib.sha256(self.binary.read_bytes()).hexdigest()}
        self.write_manifest()

    def write_manifest(self):
        self.manifest.write_text(json.dumps(self.value))

    def arguments(self):
        return SimpleNamespace(output=self.root / "output", clients=2, repositories=2,
                               identity=self.identity, member=self.value, member_manifest=self.manifest)

    def git_only(self, command, **kwargs):
        if command[0] == "git":
            return self.real_run(command, **kwargs)
        raise AssertionError("must not execute the member or compiler")

    def test_clean_exact_source_and_profile_are_required(self):
        self.assertEqual(fleet.member_manifest(self.manifest, self.identity), self.value)
        for field, value in [("commit", "a" * 40), ("tree", "b" * 40),
                             ("lock_sha256", "c" * 64), ("toolchain_sha256", "d" * 64),
                             ("dirty", True)]:
            with self.subTest(field=field), self.assertRaises(ValueError):
                fleet.member_manifest(self.manifest, self.identity | {field: value})

    def test_changed_tracked_and_untracked_inputs_cannot_use_old_artifact(self):
        (self.source / "Cargo.lock").write_text("different lock\n")
        with self.assertRaises(ValueError):
            fleet.member_manifest(self.manifest, fleet.source_identity())
        (self.source / "Cargo.lock").write_text("generated lock\n")
        (self.source / "new-source.rs").write_text("uncommitted input")
        with self.assertRaises(ValueError):
            fleet.member_manifest(self.manifest, fleet.source_identity())

    def test_user_status_preferences_cannot_hide_untracked_build_inputs(self):
        subprocess.run(["git", "config", "status.showUntrackedFiles", "no"],
                       cwd=self.source, check=True)
        (self.source / "untracked-build-input.rs").write_text("generated input")
        self.assertEqual(subprocess.check_output(["git", "status", "--porcelain"],
                                                cwd=self.source, text=True), "")
        self.assertTrue(fleet.source_identity()["dirty"])
        with self.assertRaises(ValueError):
            fleet.member_manifest(self.manifest, fleet.source_identity())

    def test_wrong_feature_test_schema_and_path_cannot_select_another_member(self):
        for key, value in [("schema", 2), ("features", "commit-sync"), ("test", "another_test"),
                           ("executable", "../other"), ("executable_sha256", "not-a-digest")]:
            with self.subTest(key=key):
                original = self.value[key]
                self.value[key] = value
                self.write_manifest()
                with self.assertRaises(ValueError):
                    fleet.member_manifest(self.manifest, self.identity)
                self.value[key] = original

    def test_corrupt_bytes_are_refused_before_execution_or_fixture_stop(self):
        self.binary.write_bytes(b"replacement bytes")
        fixture = self.root / "fixture.json"
        fixture.write_text("private fixture receipt remains unread")
        work = self.root / "work"
        work.mkdir()
        with patch.object(fleet.sys, "argv", ["periodic-fleet.py", "--fixture", str(fixture),
                          "--output", str(self.root / "output"), "--work-root", str(work),
                          "--member-manifest", str(self.manifest), "--clients", "2", "--repositories", "2"]), \
                patch.object(fleet.subprocess, "run", side_effect=self.git_only), \
                self.assertRaisesRegex(ValueError, "bytes differ"):
            fleet.main()
        self.assertEqual(fixture.read_text(), "private fixture receipt remains unread")
        self.assertFalse(fixture.with_suffix(".stop").exists())
        self.assertEqual((self.root / "output/native-fleet-member").stat().st_mode & 0o111, 0)

    def test_prebuilt_path_never_builds_and_checks_exact_test_without_running_it(self):
        listed = SimpleNamespace(stdout=f"{fleet.TEST}: test\n\n1 test, 0 benchmarks\n")
        def run_member(command, **kwargs):
            return self.real_run(command, **kwargs) if command[0] == "git" else listed
        with patch.object(fleet, "build_member", side_effect=AssertionError("must not build")), \
                patch.object(fleet.subprocess, "run", side_effect=run_member) as run:
            staged, digest = fleet.prepare_execution(self.arguments())
        self.assertEqual(staged.read_bytes(), self.binary.read_bytes())
        self.assertEqual(digest, self.value["executable_sha256"])
        self.assertEqual(staged.stat().st_mode & 0o777, 0o500)
        self.assertEqual(run.call_args.args[0], [str(staged), "--exact", fleet.TEST, "--ignored", "--list"])

    def test_missing_owning_test_is_refused(self):
        def run_member(command, **kwargs):
            return (self.real_run(command, **kwargs) if command[0] == "git"
                    else SimpleNamespace(stdout="0 tests, 0 benchmarks\n"))
        with patch.object(fleet.subprocess, "run", side_effect=run_member), \
                self.assertRaisesRegex(ValueError, "exact owning fleet test"):
            fleet.prepare_execution(self.arguments())

    def test_existing_staged_member_is_never_replaced(self):
        output = self.root / "output"
        output.mkdir()
        existing = output / "native-fleet-member"
        existing.write_bytes(b"retained previous output")
        with self.assertRaises(FileExistsError):
            fleet.stage_member(self.binary, output, self.value["executable_sha256"])
        self.assertEqual(existing.read_bytes(), b"retained previous output")

    def test_manifest_and_empty_binary_have_finite_bounds(self):
        self.manifest.write_bytes(b" " * 65537)
        with self.assertRaisesRegex(ValueError, "bound"):
            fleet.member_manifest(self.manifest, self.identity)
        self.binary.write_bytes(b"")
        output = self.root / "output"
        output.mkdir()
        with self.assertRaisesRegex(ValueError, "bound"):
            fleet.stage_member(self.binary, output)


if __name__ == "__main__":
    unittest.main()

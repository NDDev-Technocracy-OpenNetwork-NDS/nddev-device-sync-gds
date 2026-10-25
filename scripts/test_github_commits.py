"""Pure fixture admission checks; these are not GitHub provider acceptance."""
import importlib.util
from pathlib import Path
import sys
import unittest

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("fixture", Path(__file__).with_name("check-github-commits.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class Admission(unittest.TestCase):
    def setUp(self):
        self.environment = {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted",
                            "GITHUB_REPOSITORY": fixture.REPOSITORY, "GITHUB_REF": "refs/heads/main",
                            "GITHUB_RUN_ID": "123", "GITHUB_RUN_ATTEMPT": "1", "GITHUB_SHA": "a" * 40}

    def test_exact_workflow_source_and_attempt_are_retained(self):
        self.assertEqual(fixture.context(self.environment), ("123-1", "a" * 40))

    def test_local_private_fork_and_pull_request_contexts_cannot_lend_authority(self):
        for key, value in [("GITHUB_ACTIONS", "false"), ("RUNNER_ENVIRONMENT", "self-hosted"),
                           ("GITHUB_REPOSITORY", "generated/other"), ("GITHUB_REF", "refs/pull/1/merge")]:
            with self.subTest(key=key), self.assertRaises(ValueError):
                fixture.context(self.environment | {key: value})

    def test_mutable_and_path_injecting_identities_are_refused(self):
        for key, value in [("GITHUB_SHA", "main"), ("GITHUB_SHA", "a" * 41),
                           ("GITHUB_RUN_ID", "../main"), ("GITHUB_RUN_ATTEMPT", "1/heads/main")]:
            with self.subTest(key=key), self.assertRaises(ValueError):
                fixture.context(self.environment | {key: value})


if __name__ == "__main__":
    unittest.main()

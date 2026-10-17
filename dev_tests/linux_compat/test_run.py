#!/usr/bin/env python3

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

"""Unit and subprocess-lifecycle tests; no external downloads or LiteBox build."""

import json
import os
from pathlib import Path
import signal
import sys
import tempfile
import time
import unittest
from unittest import mock

import run


CASE = "Example.Works"


class HarnessTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)

    def result(self, stdout="", stderr="", returncode=0, timed_out=False, error=None):
        out = self.directory / "stdout.log"
        err = self.directory / "stderr.log"
        out.write_text(stdout)
        err.write_text(stderr)
        return run.ProcessResult([], returncode, timed_out, 0.1, str(out), str(err), error)

    def classify(self, stdout="", **kwargs):
        return run.classify(self.result(stdout, **kwargs), CASE)["status"]

    def test_listing_includes_parameterized_cases_and_ignores_comments(self):
        self.assertEqual(
            run.parse_test_list(
                "Example.\n  Works\nTyped/Fixture/0.  # TypeParam = int\n"
                "  Value/1  # GetParam() = 42\n"
            ),
            [CASE, "Typed/Fixture/0.Value/1"],
        )

    def test_empty_duplicate_or_malformed_listing_is_an_error(self):
        for listing in ("", "Example.\n  Works\n  Works\n", "Example.\n  bad name\n"):
            with self.subTest(listing=listing), self.assertRaises(run.HarnessError):
                run.parse_test_list(listing)

    def test_every_discovered_case_must_be_selected_or_explained(self):
        definition = {"binary": "example", "cases": [CASE], "excluded": {"Other.Test": "root"}}
        run.validate_selection(definition, [CASE, "Other.Test"])
        for discovered in ([CASE], [CASE, "Other.Test", "New.Test"]):
            with self.subTest(discovered=discovered), self.assertRaises(run.HarnessError):
                run.validate_selection(definition, discovered)

    def test_selection_rejects_duplicates_overlap_empty_and_unexplained_exclusions(self):
        for selected, excluded in (
            ([], {}), ([CASE, CASE], {}), ([CASE], {CASE: "reason"}),
            ([CASE], {"Other.Test": ""}),
        ):
            with self.subTest(selected=selected, excluded=excluded):
                with self.assertRaises(run.HarnessError):
                    run.validate_selection(
                        {"binary": "example", "cases": selected, "excluded": excluded},
                        [CASE, "Other.Test"],
                    )

    def test_success_requires_exactly_the_requested_test(self):
        self.assertEqual(self.classify(
            f"[ RUN      ] {CASE}\n[       OK ] {CASE} (0 ms)\n",
        ), "pass")
        for output in (
            "", "[==========] Running 0 tests.\n",
            f"[ RUN      ] {CASE}\n",
            "[ RUN      ] Other.Test\n[       OK ] Other.Test (0 ms)\n",
            f"[ RUN      ] {CASE}\n[ RUN      ] Other.Test\n[       OK ] {CASE} (0 ms)\n",
        ):
            with self.subTest(output=output):
                self.assertEqual(self.classify(output), "infrastructure_error")

    def test_failure_including_repeated_google_test_summary(self):
        self.assertEqual(self.classify(
            f"[ RUN      ] {CASE}\n[  FAILED  ] {CASE} (0 ms)\n[  FAILED  ] {CASE}\n",
            returncode=1,
        ), "failure")
        self.assertEqual(self.classify(
            f"[ RUN      ] {CASE}\n[  FAILED  ] {CASE} (0 ms)\n",
        ), "infrastructure_error")

    def test_skip_is_not_a_pass(self):
        self.assertEqual(self.classify(
            f"[ RUN      ] {CASE}\n[  SKIPPED ] {CASE} (0 ms)\n",
        ), "skip")

    def test_crash_timeout_and_launcher_failure(self):
        for stderr in (
            "thread 'main' panicked at src/lib.rs:123",
            "Error: runner exited with signal: 6 (SIGABRT)",
        ):
            with self.subTest(stderr=stderr):
                self.assertEqual(self.classify(stderr=stderr, returncode=1), "crash")
        self.assertEqual(self.classify(returncode=-signal.SIGSEGV), "crash")
        self.assertEqual(self.classify(
            f"[ RUN      ] {CASE}\n", returncode=1,
        ), "crash")
        self.assertEqual(self.classify(
            f"[ RUN      ] {CASE}\n[       OK ] {CASE} (0 ms)\n", returncode=1,
        ), "crash")
        self.assertEqual(self.classify(returncode=-9, timed_out=True), "timeout")
        self.assertEqual(self.classify(returncode=1, stderr="missing library"), "infrastructure_error")
        self.assertEqual(self.classify(returncode=None, error="not found"), "infrastructure_error")

    def test_environment_does_not_inherit_filters_or_gvisor_identity(self):
        with mock.patch.dict(os.environ, {"GTEST_FILTER": "*", "TEST_ON_GVISOR": "systrap"}):
            environment = run.test_environment(self.directory)
        self.assertNotIn("GTEST_FILTER", environment)
        self.assertNotIn("TEST_ON_GVISOR", environment)
        self.assertEqual(environment["TEST_TMPDIR"], str(self.directory))

    def test_exit_status_and_score_do_not_hide_native_failures_or_skips(self):
        report = {
            "manifest": {"tests": [{"excluded": {"Root.Test": "needs root"}}]},
            "infrastructure_errors": [], "results": [],
        }
        self.assertEqual(run.report_exit_code(report), 2)
        report["results"] = [
            {"binary": "test", "case": "Fail", "native": {"status": "failure"},
             "litebox": {"status": "pass"}},
            {"binary": "test", "case": "Skip", "native": {"status": "pass"},
             "litebox": {"status": "skip"}},
            {"binary": "test", "case": "Pass", "native": {"status": "pass"},
             "litebox": {"status": "pass"}},
        ]
        self.assertEqual(run.report_exit_code(report), 1)
        run.write_report(report, self.directory)
        saved = json.loads((self.directory / "results.json").read_text())
        self.assertEqual(saved["summary"]["native_pass_cases"], 2)
        self.assertEqual(saved["summary"]["litebox_pass_among_native_pass"], 1)
        self.assertEqual(saved["summary"]["statuses"]["native"]["failure"], 1)
        self.assertEqual(saved["summary"]["excluded_cases"], 1)
        self.assertIn("| test | Fail | failure | pass |", (self.directory / "summary.md").read_text())
        report["infrastructure_errors"] = ["build failed"]
        self.assertEqual(run.report_exit_code(report), 2)

    def test_packaging_includes_runfiles_and_helper_library_discovery(self):
        binary = self.directory / "example"
        binary.write_bytes(b"\x7fELFmain")
        runfiles = self.directory / "example.runfiles"
        runfiles.mkdir()
        (runfiles / "helper").write_bytes(b"\x7fELFhelper")
        (runfiles / "data").write_text("fixture")
        (runfiles / "MANIFEST").write_text("host-only paths")
        with mock.patch.object(run, "checked") as checked:
            run.package_test(binary, self.directory / "rootfs.tar", self.directory,
                             self.directory / "logs")
        command = checked.call_args.args[0]
        self.assertIn(str(runfiles / "helper"), command)
        self.assertIn(f"{runfiles / 'data'}:{runfiles / 'data'}", command)
        self.assertNotIn(str(runfiles / "MANIFEST"), " ".join(command))

    def test_process_exit_and_spawn_failure_are_logged(self):
        result = run.run_process(
            [sys.executable, "-c", "import sys; print('output'); sys.exit(3)"],
            self.directory / "exit", cwd=self.directory,
        )
        self.assertEqual(result.returncode, 3)
        self.assertEqual(run.read_log(result.stdout), "output\n")
        self.assertTrue((self.directory / "exit/process.json").is_file())
        missing = run.run_process(
            [str(self.directory / "does-not-exist")], self.directory / "missing",
            cwd=self.directory,
        )
        self.assertIsNotNone(missing.error)
        self.assertIsNone(missing.returncode)

    def assert_stopped(self, pid):
        for _ in range(100):
            status = Path(f"/proc/{pid}/stat")
            try:
                state = status.read_text().rsplit(")", 1)[1].split()[0]
            except FileNotFoundError:
                return
            if state == "Z":
                return  # An orphan may await reaping by the host's init.
            time.sleep(0.01)
        self.fail(f"Descendant {pid} survived process-group cleanup")

    def test_timeout_kills_children_and_allows_the_next_command(self):
        script = (
            "import subprocess,sys,time; "
            "child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)']); "
            "print(child.pid,flush=True); time.sleep(60)"
        )
        result = run.run_process(
            [sys.executable, "-c", script], self.directory / "timeout",
            cwd=self.directory, timeout=2,
        )
        self.assertTrue(result.timed_out)
        self.assert_stopped(int(run.read_log(result.stdout)))
        next_result = run.run_process(
            [sys.executable, "-c", "pass"], self.directory / "next", cwd=self.directory,
        )
        self.assertEqual(next_result.returncode, 0)

    def test_successful_leader_does_not_leave_running_children(self):
        result = run.run_process(
            [sys.executable, "-c",
             "import subprocess,sys; "
             "child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)']); "
             "print(child.pid,flush=True)"],
            self.directory / "orphan", cwd=self.directory,
        )
        self.assertEqual(result.returncode, 0)
        self.assert_stopped(int(run.read_log(result.stdout)))


if __name__ == "__main__":
    unittest.main()

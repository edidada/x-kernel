# SPDX-License-Identifier: Apache-2.0
"""Acceptance-boundary regressions, independent of any production crate."""

from pathlib import Path
import tempfile
import unittest

from reports import audit_report, check_reports, check_sources, digest

NAME = "verification::example"
NORMAL = '''Check 1: verification::example.assertion.1
 - Status: SUCCESS
 - Description: "value is correct"
Check 2: verification::example.cover.1
 - Status: SATISFIED
 - Description: "boundary reachable"
VERIFICATION:- SUCCESSFUL
'''
PANIC = '''Check 1: verification::example.cover.1
 - Status: SATISFIED
 - Description: "panic-domain"
Check 2: Example::clamp.assertion.1
 - Status: FAILURE
 - Description: "assertion failed: min <= max"
 - Location: example.rs:1:1 in function Example::clamp
Check 3: verification::example.cover.2
 - Status: UNREACHABLE
 - Description: "panic-return"
VERIFICATION:- SUCCESSFUL (encountered one or more panics as expected)
'''
POLICY = {"panic": True, "universal": True, "functions": ["Example::clamp"],
          "assertion": ".assertion.1", "description": "assertion failed: min <= max"}


class ReportTests(unittest.TestCase):
    def test_normal_success_and_multiline_description(self):
        audit_report(NORMAL, NAME, {"panic": False})
        audit_report(NORMAL.replace("value is correct", "value is\n    correct"), NAME, {"panic": False})

    def test_universal_panic(self):
        audit_report(PANIC, NAME, POLICY)
        audit_report(PANIC.replace("UNREACHABLE", "UNSATISFIABLE"), NAME, POLICY)

    def test_surviving_illegal_input(self):
        with self.assertRaises(ValueError):
            audit_report(PANIC.replace("UNREACHABLE", "SATISFIED"), NAME, POLICY)

    def test_empty_illegal_domain(self):
        with self.assertRaises(ValueError):
            audit_report(PANIC.replace("Status: SATISFIED", "Status: UNREACHABLE"), NAME, POLICY)

    def test_unknown_return(self):
        for status in ("UNKNOWN", "UNDETERMINED", "UNCOVERED"):
            with self.subTest(status=status), self.assertRaises(ValueError):
                audit_report(PANIC.replace("UNREACHABLE", status), NAME, POLICY)

    def test_wrong_failure_site_description_or_assertion(self):
        for old, new in (("Example::clamp", "Other::clamp"), (".assertion.1", ".assertion.2"),
                         ("min <= max", "different assertion"), (".assertion.1", ".overflow.1")):
            with self.subTest(old=old), self.assertRaises(ValueError):
                audit_report(PANIC.replace(old, new), NAME, POLICY)

    def test_missing_panic(self):
        with self.assertRaises(ValueError):
            audit_report(PANIC.replace("Status: FAILURE", "Status: SUCCESS"), NAME, POLICY)

    def test_missing_or_duplicate_marker(self):
        for changed in (PANIC.replace("panic-return", "other"), PANIC.replace("panic-return", "panic-domain")):
            with self.assertRaises(ValueError):
                audit_report(changed, NAME, POLICY)

    def test_unreachable_normal_cover(self):
        with self.assertRaises(ValueError):
            audit_report(NORMAL.replace("SATISFIED", "UNREACHABLE"), NAME, {"panic": False})

    def test_normal_failed_or_unknown_check(self):
        for status in ("FAILURE", "UNKNOWN", "UNDETERMINED"):
            with self.assertRaises(ValueError):
                audit_report(NORMAL.replace("Status: SUCCESS", "Status: " + status), NAME, {"panic": False})

    def test_incomplete_reports(self):
        for changed in (NORMAL.split("VERIFICATION:")[0],
                        NORMAL.replace("Status: SUCCESS", "Status was SUCCESS")):
            with self.assertRaises(ValueError):
                audit_report(changed, NAME, {"panic": False})

    def test_concrete_panic_is_not_universal(self):
        policy = dict(POLICY, universal=False)
        witness = PANIC.replace("UNREACHABLE", "SATISFIED")
        audit_report(witness, NAME, policy)

    def test_complete_run_and_filtered_run(self):
        self.check_run()
        self.check_run(config={"harnesses": {NAME: {"panic": False}, "extra": {"panic": False}}}, filtered=True)

    def check_run(self, *, log=None, config=None, filtered=False, missing=False, extra=False):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "verification.log").write_text(log if log is not None else f"Checking harness {NAME}...\n")
            (root / "result_output_dir").mkdir()
            if not missing:
                (root / "result_output_dir" / NAME).write_text(NORMAL)
            if extra:
                (root / "result_output_dir/unused").write_text(NORMAL)
            return check_reports(root, config or {"harnesses": {NAME: {"panic": False}}}, filtered=filtered)

    def test_missing_duplicate_unknown_harnesses(self):
        for log in ("", f"Checking harness {NAME}...\n" * 2, "Checking harness unknown...\n"):
            with self.assertRaises(ValueError):
                self.check_run(log=log)
        with self.assertRaises(ValueError):
            self.check_run(log="", filtered=True)

    def test_missing_and_stale_report_files(self):
        with self.assertRaises(ValueError):
            self.check_run(missing=True)
        with self.assertRaises(ValueError):
            self.check_run(extra=True)

    def test_unfiltered_run_requires_all_harnesses(self):
        with self.assertRaises(ValueError):
            self.check_run(config={"harnesses": {NAME: {"panic": False}, "extra": {"panic": False}}})

    def test_source_changes_require_review(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "example/src").mkdir(parents=True)
            source = root / "example/src/lib.rs"
            manifest = root / "example/Cargo.toml"
            source.write_text("original")
            manifest.write_text("manifest")
            config = {"production": "example", "production_sha256": {
                str(p.relative_to(root)): digest(p) for p in (source, manifest)}}
            check_sources(root, config)
            source.write_text("changed")
            with self.assertRaises(ValueError):
                check_sources(root, config)
            source.write_text("original")
            (source.parent / "new.rs").write_text("new API")
            with self.assertRaises(ValueError):
                check_sources(root, config)


if __name__ == "__main__":
    unittest.main()

"""scripts/audit-bin.py against a fake cargo-audit:
python3 -m unittest discover -s scripts/tests -p 'test_*.py'."""
import contextlib
import importlib.util
import io
import os
from pathlib import Path
import tempfile
import tomllib
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location(
    "audit_bin", Path(__file__).parents[1] / "audit-bin.py")
audit = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audit)

# Prints the status line cargo-audit prints for a binary carrying a record, for each
# argument named in $FOUND, then $EXTRA, records its arguments, and exits with $RC.
FAKE = """#!/bin/sh
printf '%s\\n' "$@" > "$CALLS"
for b in $FOUND; do echo "       Found 'cargo auditable' data in $b (412 dependencies)"; done
[ -z "$EXTRA" ] || echo "$EXTRA"
exit "${RC:-0}"
"""
INDEX_DOWN = "warning: couldn't update crates.io index: failed to fetch"


class AuditCommand(unittest.TestCase):
    def test_deny_toml_rules_become_flags(self):
        cmd = audit.audit_command({
            "unmaintained": "all",
            "yanked": "deny",
            "ignore": [{"id": "RUSTSEC-2024-0436", "reason": "r"}, "RUSTSEC-2023-0071"],
        }, ["a", "b"])
        self.assertEqual(cmd, [
            "cargo-audit", "audit", "--color", "never",
            "--deny", "unmaintained", "--deny", "unsound", "--deny", "yanked",
            "--ignore", "RUSTSEC-2024-0436", "--ignore", "RUSTSEC-2023-0071",
            "bin", "--max-binary-size", "18446744073709551615", "a", "b",
        ])

    def test_only_none_leaves_a_kind_a_warning(self):
        cmd = audit.audit_command({"unmaintained": "none", "unsound": "workspace"}, ["a"])
        self.assertNotIn("unmaintained", cmd)
        self.assertIn("unsound", cmd)

    def test_a_yanked_crate_fails_only_when_deny_toml_denies_it(self):
        denied = audit.audit_command({"yanked": "deny"}, ["a"])
        self.assertEqual(denied[denied.index("yanked") - 1], "--deny")
        for setting in ({}, {"yanked": "warn"}, {"yanked": "allow"}):
            self.assertNotIn("yanked", audit.audit_command(setting, ["a"]))

    def test_an_ignore_by_crate_has_no_cargo_audit_equivalent(self):
        with self.assertRaises(SystemExit):
            audit.audit_command({"ignore": [{"crate": "paste"}]}, ["a"])

    def test_the_real_deny_toml_translates(self):
        deny = tomllib.loads((Path(__file__).parents[2] / "deny.toml").read_text())
        audit.audit_command(deny["advisories"], ["a"])


class Main(unittest.TestCase):
    def setUp(self):
        tmp = Path(self.enterContext(tempfile.TemporaryDirectory()))
        (tmp / "cargo-audit").write_text(FAKE)
        (tmp / "cargo-audit").chmod(0o755)
        self.calls = tmp / "calls"
        self.env = {"PATH": f"{tmp}:{os.environ['PATH']}", "CALLS": str(self.calls)}

        self.deny = tmp / "deny.toml"
        self.deny.write_text('[advisories]\nyanked = "deny"\n')

    def run_main(self, binaries, **env):
        out = io.StringIO()
        with mock.patch.dict(os.environ, {**self.env, **env}), \
                mock.patch.object(audit, "DENY_TOML", self.deny), \
                contextlib.redirect_stdout(out), contextlib.redirect_stderr(out):
            return audit.main(binaries), out.getvalue()

    def test_a_yanked_check_that_could_not_run_fails_when_yanked_is_denied(self):
        for line in (INDEX_DOWN,
                     "warning: couldn't open crates.io index: lock timeout",
                     "error: couldn't check if the package is yanked: not found"):
            with self.subTest(line=line):
                rc, out = self.run_main(["bin/a"], FOUND="bin/a", EXTRA=line)
                self.assertEqual(rc, 1)
                self.assertIn("could not check for yanked crates", out)

    def test_a_yanked_check_that_could_not_run_only_warns_when_yanked_warns(self):
        self.deny.write_text('[advisories]\nyanked = "warn"\n')
        rc, out = self.run_main(["bin/a"], FOUND="bin/a", EXTRA=INDEX_DOWN)
        self.assertEqual(rc, 0, out)

    def test_binaries_with_records_and_no_findings_pass(self):
        rc, out = self.run_main(["bin/a", "bin/b"], FOUND="bin/a bin/b")
        self.assertEqual(rc, 0, out)
        args = self.calls.read_text().split()
        self.assertEqual(args[:2], ["audit", "--color"])
        self.assertEqual(args[-2:], ["bin/a", "bin/b"])

    def test_a_binary_without_a_record_fails(self):
        rc, out = self.run_main(["bin/a", "bin/b"], FOUND="bin/a")
        self.assertEqual(rc, 1)
        self.assertIn("bin/b: no cargo-auditable record", out)
        self.assertNotIn("bin/a: no", out)

    def test_a_finding_fails(self):
        rc, _ = self.run_main(["bin/a"], FOUND="bin/a", RC="1")
        self.assertEqual(rc, 1)

    def test_no_binary_is_a_usage_error(self):
        rc, _ = self.run_main([])
        self.assertEqual(rc, 2)
        self.assertFalse(self.calls.exists())


if __name__ == "__main__":
    unittest.main()

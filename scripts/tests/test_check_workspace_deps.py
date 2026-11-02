"""scripts/check-workspace-deps.py on scratch workspaces:
python3 -m unittest discover -s scripts/tests -p 'test_*.py'."""
import contextlib
import importlib.util
import io
from pathlib import Path
import tempfile
import tomllib
import unittest

spec = importlib.util.spec_from_file_location(
    "check_workspace_deps", Path(__file__).parents[1] / "check-workspace-deps.py")
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)


def entries(text):
    return check.inline_entries(tomllib.loads(text))


class InlineEntries(unittest.TestCase):
    def test_workspace_references_pass(self):
        self.assertEqual(entries("""
[dependencies]
serde.workspace = true
zip = { workspace = true, features = ["time"] }
[dev-dependencies]
me = { workspace = true, features = ["test-util"] }
[target.'cfg(unix)'.dependencies]
libc.workspace = true
"""), [])

    def test_every_other_form_is_listed(self):
        self.assertEqual(entries("""
[dependencies]
rand = "0.8"
zip = { version = "2", features = ["time"] }
local = { path = "../local" }
[dev-dependencies]
remote = { git = "https://example.invalid/remote" }
[build-dependencies]
cc = { workspace = false, version = "1" }
[target.'cfg(windows)'.dependencies]
winapi = "0.3"
"""), [
            "dependencies.rand",
            "dependencies.zip",
            "dependencies.local",
            "dev-dependencies.remote",
            "build-dependencies.cc",
            "target.cfg(windows).dependencies.winapi",
        ])


class Main(unittest.TestCase):
    def workspace(self, root, members):
        # A glob and a literal path, as the root Cargo.toml writes members.
        (root / "Cargo.toml").write_text(
            '[workspace]\nmembers = ["crates/*", "apps/server"]\nexclude = ["crates/vendored"]\n')
        for name, deps in members.items():
            (root / name).mkdir(parents=True)
            (root / name / "Cargo.toml").write_text(f"[dependencies]\n{deps}\n")

    def run_main(self, root):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = check.main(["check-workspace-deps.py", str(root)])
        return code, out.getvalue()

    def test_a_clean_workspace_passes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.workspace(root, {"crates/a": "serde.workspace = true",
                                  "apps/server": "serde.workspace = true",
                                  "crates/vendored": 'serde = "1"'})
            self.assertEqual(self.run_main(root), (0, ""))

    def test_one_inline_version_fails_and_is_named(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.workspace(root, {"crates/a": "serde.workspace = true",
                                  "apps/server": 'rand = "0.8"'})
            code, out = self.run_main(root)
            self.assertEqual(code, 1)
            self.assertIn("apps/server/Cargo.toml: dependencies.rand", out)
            self.assertNotIn("crates/a", out)

    def test_a_root_without_members_is_an_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "Cargo.toml").write_text('[workspace]\nmembers = ["crates/*"]\n')
            code, out = self.run_main(root)
            self.assertEqual(code, 2)
            self.assertIn("lists no workspace member manifest", out)


if __name__ == "__main__":
    unittest.main()

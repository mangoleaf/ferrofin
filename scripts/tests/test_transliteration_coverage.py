"""Citation matching of scripts/transliteration-coverage.py, on a tiny fake upstream and
Rust tree: python3 -m unittest discover -s scripts/tests -p 'test_*.py'."""
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).parents[1] / "transliteration-coverage.py"
spec = importlib.util.spec_from_file_location("transliteration_coverage", SCRIPT)
tc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tc)

UPSTREAM_CS = """
public class LocalizationManagerTests
{
    [Theory]
    [InlineData("G", 0)]
    [InlineData("PG", 10)]
    // [InlineData("X", 99)]
    public void GetRatingLevel_GivenValidString_Success(string value, int level) { }

    [Fact]
    public void GetCountries_All_Success() { }
}
"""

RUST = """
/// Upstream `LocalizationManagerTests`, every `[InlineData]`.
#[rstest]
#[case("G", 0)]
#[case("PG", 10)]
fn cited_by_class(#[case] v: &str, #[case] l: i32) {}

#[rstest]
#[case(1)]
fn get_rating_level_given_valid_string_success(#[case] v: i32) {}

// Upstream LocalizationManagerTests too, but a blank line detaches it.

#[rstest]
#[case(1)]
#[case(2)]
fn not_cited(#[case] v: i32) {}

/// Names `GetCountries_All_Success` but has no cases.
#[test]
fn no_cases() {}
"""


class TransliterationCoverage(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        cs = root / "up/tests/Jellyfin.Server.Implementations.Tests/Localization"
        cs.mkdir(parents=True)
        (cs / "LocalizationManagerTests.cs").write_text(UPSTREAM_CS)
        (root / "up/tests/Jellyfin.Naming.Tests").mkdir()
        (root / "src").mkdir()
        (root / "src/lib.rs").write_text(RUST)
        self.root = root

    def tearDown(self):
        self.tmp.cleanup()

    def test_index_names_classes_and_test_methods_and_counts_inline_data(self):
        names, inline = tc.upstream_index(self.root / "up")
        project = "Jellyfin.Server.Implementations.Tests"
        self.assertEqual(names["LocalizationManagerTests"], project)
        self.assertEqual(names["GetRatingLevel_GivenValidString_Success"], project)
        self.assertEqual(names["GetCountries_All_Success"], project)
        self.assertEqual(inline, {project: 2, "Jellyfin.Naming.Tests": 0})

    def test_counts_cases_cited_by_attached_comment_or_test_name_only(self):
        names, inline = tc.upstream_index(self.root / "up")
        counts = tc.rust_cases([self.root / "src"], names)
        self.assertEqual(counts, {"Jellyfin.Server.Implementations.Tests": 3})
        self.assertEqual(tc.report(inline, counts)[-1], "| **total** | **3** | **2** |")

    def test_skips_without_a_checkout(self):
        env = {k: v for k, v in os.environ.items() if k != "FERROFIN_UPSTREAM_DIR"}
        out = subprocess.run([sys.executable, str(SCRIPT)], env=env, capture_output=True,
                             text=True, check=True).stdout
        self.assertTrue(out.startswith("SKIP: set FERROFIN_UPSTREAM_DIR"), out)


if __name__ == "__main__":
    unittest.main()

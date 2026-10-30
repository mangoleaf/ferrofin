"""baseline.py: the previous release a release's A/B gate compares against."""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import baseline  # noqa: E402

TAGS = ["v1.0.0", "v1.2.0", "v1.3.0", "v1.3.2", "v1.3.10-rc1", "v1.4.0", "v1.30.0", "v10.0.0", "chart-v1.3.9"]


class Baseline(unittest.TestCase):
    def test_a_release_compares_against_the_latest_release_of_the_minor_line_before_it(self):
        for release, want in [("v1.4.1", "v1.3.2"), ("v1.4.0", "v1.3.2"), ("v1.4.7", "v1.3.2"), ("v1.5.0", "v1.4.0"),
                              ("v2.0.0", "v1.30.0"), ("v1.31.0", "v1.30.0"), ("v1.3.5", "v1.2.0"), ("v11.0.0", "v10.0.0")]:
            with self.subTest(release):
                self.assertEqual(baseline.baseline(release, TAGS), want)

    def test_versions_sort_by_number_and_only_final_tags_count(self):
        self.assertEqual(baseline.baseline("v1.4.1", ["v1.3.2", "v1.3.10", "v1.3.11-rc1"]), "v1.3.10")
        self.assertIsNone(baseline.baseline("v1.4.1", ["v1.3.11-rc1", "v1.30.0"]))   # v1.30 is not the v1.3 line
        self.assertIsNone(baseline.baseline("v0.0.3", TAGS))   # nothing before the first line

    def test_the_tag_of_an_image_reference(self):
        for image, tag in [("ghcr.io/mangoleaf/ferrofin:v1.3.2", "v1.3.2"), ("localhost:5000/ferrofin:v1.3.2", "v1.3.2"),
                           ("ghcr.io/mangoleaf/ferrofin:v1.3.2@sha256:abc", "v1.3.2"), ("localhost:5000/ferrofin", ""),
                           ("ferrofin@sha256:abc", ""), ("ferrofin:old", "old")]:
            with self.subTest(image):
                self.assertEqual(baseline.tag_of(image), tag)

    def test_the_release_is_the_flag_else_a_release_branch_else_none(self):
        self.assertEqual(baseline.release_of("release/v1.4.1"), "v1.4.1")
        self.assertEqual(baseline.release_of("main", "v1.4.2"), "v1.4.2")   # a patch cut from main
        self.assertEqual(baseline.release_of("release/v1.4.1", "v1.4.2"), "v1.4.2")
        for branch in ("main", "feat/ab-work", "release/v1.5.0-rc", "release/next", "detached-0123abcd", ""):
            with self.subTest(branch):
                self.assertIsNone(baseline.release_of(branch))
        with self.assertRaisesRegex(ValueError, "--release 1.4.2 is not a release version"):
            baseline.release_of("main", "1.4.2")

    def test_the_problem_names_the_image_to_use(self):
        self.assertIsNone(baseline.problem("ghcr.io/mangoleaf/ferrofin:v1.3.2", "v1.4.1", TAGS))
        self.assertIsNone(baseline.problem("ferrofin:v1.3.2@sha256:abc", "v1.4.1", TAGS))
        self.assertIsNone(baseline.problem("ferrofin:anything", None, TAGS))   # no release: any image
        self.assertEqual(baseline.problem("ghcr.io/mangoleaf/ferrofin:v1.4.0", "v1.4.1", TAGS),
                         "the release A/B gate of v1.4.1 compares against v1.3.2, the latest v1.3 release: use "
                         "ghcr.io/mangoleaf/ferrofin:v1.3.2 as the previous image, not ghcr.io/mangoleaf/ferrofin:v1.4.0")
        self.assertEqual(baseline.problem("x:v1.3.2", "v1.4.1", []),
                         "no v1.3 release tag in this checkout to compare release v1.4.1 against: git fetch --tags")
        self.assertIn("no earlier release tag", baseline.problem("x:v0.0.1", "v0.0.2", TAGS))

    def test_the_command_line_exits_2_naming_the_image(self):
        self.assertEqual(baseline.main(["check", "ferrofin:x", "--release", "1.4"]), 2)
        with self.assertRaises(SystemExit):
            baseline.main(["check"])


if __name__ == "__main__":
    unittest.main()

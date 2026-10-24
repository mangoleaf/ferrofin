"""The Python clean-value port against the Rust `[case]` oracle in string_extensions.rs."""

from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).parents[1]))
from clean_value import get_clean_value, lower_invariant, remove_diacritics, repaired_clean_value

# From `get_clean_value_normalizes` in crates/ferrofin-util/src/string_extensions.rs.
CLEAN_VALUE = [
    ("weird: the al yankovic story", "weird the al yankovic story"),
    ("Mötley Crüe: Greatest Hits!", "motley crue greatest hits"),
    ("H. Jon Benjamin", "h jon benjamin"),
    ("Warner Bros. Pictures", "warner bros pictures"),
    ("ΟΣ", "οσ"),  # simple casing: no final-sigma context
    ("𐐀", "𐐨"),
    ("Béla   Tarr!!", "bela tarr"),
    ("A,B;C", "a b c"),
    ("Spider-Man: Across the Spider-Verse", "spider man across the spider verse"),
    ("2001: A Space Odyssey", "2001 a space odyssey"),
    ("千と千尋の神隠し", "千と千尋の神隠し"),  # CJK letters are \p{L}: untouched
    ("千と千尋の神隠し！", "千と千尋の神隠し"),  # …a fullwidth mark is not
    ("Ａｂｃ", "ａｂｃ"),  # fullwidth letters lowercase and survive
    ("  padded  ", "padded"),
    ("tab\tand\nnewline", "tab and newline"),
]

# From `remove_diacritics_valid_input_corrects` (Jellyfin's own issue cases).
REMOVE_DIACRITICS = [
    ("", ""),
    ("Indiana Jones", "Indiana Jones"),
    ("a�b", "ab"),  # U+FFFD replacement char stripped
    ("åäö", "aao"),
    ("Jön", "Jon"),
    ("Jönssonligan", "Jonssonligan"),
    ("Kieślowski", "Kieslowski"),
    ("Cidadão Kane", "Cidadao Kane"),
    ("운명처럼 널 사랑해", "운명처럼 널 사랑해"),
    ("애타는 로맨스", "애타는 로맨스"),
    ("Le cœur a ses raisons", "Le coeur a ses raisons"),
    ("Béla Tarr", "Bela Tarr"),
]

# The lower half of `invariant_casing` (input -> lower), the cases that exercise simple casing.
LOWER_INVARIANT = [
    ("alice", "alice"),
    ("MÜNCHEN", "münchen"),
    ("ÑOÑO", "ñoño"),
    ("IIİı", "iiİı"),  # capital dotted I preserved
    ("STRAßEẞ", "straßeß"),
    ("ΣΣΣ", "σσσ"),  # simple casing: every sigma lowercases to σ
    ("ПРИВЕТ", "привет"),
    ("𐐀𐐀", "𐐨𐐨"),
]


class CleanValuePort(unittest.TestCase):
    def test_get_clean_value_matches_rust_oracle(self):
        for value, expected in CLEAN_VALUE:
            with self.subTest(value=value):
                self.assertEqual(get_clean_value(value), expected)

    def test_remove_diacritics_matches_rust_oracle(self):
        for value, expected in REMOVE_DIACRITICS:
            with self.subTest(value=value):
                self.assertEqual(remove_diacritics(value), expected)

    def test_lower_invariant_matches_rust_oracle(self):
        for value, expected in LOWER_INVARIANT:
            with self.subTest(value=value):
                self.assertEqual(lower_invariant(value), expected)

    def test_all_punctuation_value_derives_to_empty(self):
        for value in ("!!!", ",", "- . -", "()"):
            with self.subTest(value=value):
                self.assertEqual(get_clean_value(value), "")

    def test_whitespace_only_value_is_returned_unchanged(self):
        self.assertEqual(get_clean_value("   "), "   ")
        self.assertEqual(get_clean_value(""), "")

    def test_repaired_clean_value_mirrors_the_boot_repair(self):
        # The repair blanks a whitespace-only Value (get_clean_value would keep it) and derives the
        # rest; a NULL Value has no derived target.
        self.assertEqual(repaired_clean_value("   "), "")
        self.assertEqual(repaired_clean_value(""), "")
        self.assertEqual(repaired_clean_value("Mötley Crüe!"), "motley crue")
        self.assertEqual(repaired_clean_value("!!!"), "")
        self.assertIsNone(repaired_clean_value(None))


if __name__ == "__main__":
    unittest.main()

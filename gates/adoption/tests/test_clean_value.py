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
    ("𐐀", ""),  # .NET's regex sees two surrogates, neither a letter
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

# From `diacritics_match_jellyfin_12_4`: (input, RemoveDiacritics, GetCleanValue) as Jellyfin 12.4's
# own assemblies give them. Combining marks are kept (only the combining cedilla goes) and cleaned to
# spaces; letters outside the Diacritics package's table are kept.
JELLYFIN_12_4 = [
    ("Zo\u00eb Salda\u00f1a", "Zoe Saldana", "zoe saldana"),
    ("Zoe\u0308 Saldan\u0303a", "Zoe\u0308 Saldan\u0303a", "zoe saldan a"),
    ("\u05e9\u05b8\u05c1\u05dc\u05d5\u05b9\u05dd", "\u05e9\u05b8\u05c1\u05dc\u05d5\u05b9\u05dd", "\u05e9 \u05dc\u05d5 \u05dd"),
    ("\u0645\u064f\u062d\u064e\u0645\u0651\u064e\u062f", "\u0645\u064f\u062d\u064e\u0645\u0651\u064e\u062f", "\u0645 \u062d \u0645 \u062f"),
    ("Stra\u00dfe", "Stra\u00dfe", "stra\u00dfe"),
    ("STRA\u1e9eE", "STRA\u00dfE", "stra\u00dfe"),
    ("\u00a1Three Amigos!", "Three Amigos!", "three amigos"),
    ("Esperanto \u0108e\u0125o", "Esperanto \u0108e\u0125o", "esperanto \u0109e\u0125o"),
    ("Fran\u0063\u0327ois", "Francois", "francois"),
    ("\u0410\u043d\u0434\u0440\u0435\u0439", "\u0410\u043d\u0434\u0440\u0435\u0438", "\u0430\u043d\u0434\u0440\u0435\u0438"),
    ("Ma\u00f1ana \u00aa \u00ba", "Manana a o", "manana a o"),
    ("\u0130stanbul \u0131", "Istanbul i", "istanbul i"),
    ("\u0110or\u0111e", "Dorde", "dorde"),
    ("\u00c6on Fl\u00f8de \u0152uvre \u0141\u00f3d\u017a", "\u00c6on Flode \u0152uvre Lodz", "\u00e6on flode \u0153uvre lodz"),
    ("\u00d0\u00f0 \u00de\u00fe", "\u00d0\u00f0 \u00de\u00fe", "\u00f0\u00f0 \u00fe\u00fe"),
    ("\u212b\u2126\u212a", "\u212b\u2126\u212a", "\u00e5\u03c9k"),
    ("\uf900", "\uf900", "\uf900"),
    ("\u0915\u093f\u0924\u093e\u092c", "\u0915\u093f\u0924\u093e\u092c", "\u0915 \u0924 \u092c"),
    ("\u00e9\u0301", "e\u0301", "e"),
    ("\U00010400 Zo\u00eb", "\U00010400 Zoe", "zoe"),  # outside the BMP: two surrogates
    ("\U00020000\u5927", "\U00020000\u5927", "\u5927"),
    ("a\U0001f600b", "a\U0001f600b", "a b"),
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

    def test_the_fold_matches_jellyfin_12_4(self):
        for value, removed, clean in JELLYFIN_12_4:
            with self.subTest(value=value):
                self.assertEqual(remove_diacritics(value), removed)
                self.assertEqual(get_clean_value(value), clean)

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

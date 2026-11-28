#!/usr/bin/env python3
"""A Python port of ``ferrofin_util::string_extensions::get_clean_value``.

The boot repair ``repair_clean_values`` (the port of Jellyfin 12.0's
``RefreshCleanNamesAsync``) rewrites every ``ItemValues.CleanValue`` to
``get_clean_value(Value)`` during adoption, so the table-digest check needs the
same derivation to tell a legitimate rewrite from damage. This mirrors the Rust
function (``crates/ferrofin-util/src/string_extensions.rs``) exactly: diacritic
removal with the same table (the Diacritics package's, as Jellyfin uses it), .NET invariant
*simple* lowercasing (ICU's per-character mapping, via PyICU — not Python's full
``str.lower()``, which applies contextual rules the .NET mapping does not, e.g.
Greek final sigma), then punctuation to spaces, collapse and trim. It is proven
against the Rust ``[case]`` oracle in ``tests/test_clean_value.py``.
"""

import unicodedata

import icu

# unicodedata (General_Category) and ICU (simple casing) here may be a different Unicode
# version from the Rust regex/icu_casemap crates, but only a newly assigned code point could
# differ, so the oracle in tests/test_clean_value.py and the live adoption run cover the risk.

# DIACRITICS in string_extensions.rs: the table of the Diacritics package Jellyfin's
# RemoveDiacritics calls, in code point order (U+00DF maps to itself and is left out). The package
# replaces each character it lists and normalises nothing, so combining marks stay.
DIACRITICS = {
    "\u00a1": "",
    "\u00aa": "a",
    "\u00ba": "o",
    "\u00bf": "",
    "\u00c0": "A",
    "\u00c1": "A",
    "\u00c2": "A",
    "\u00c3": "A",
    "\u00c4": "A",
    "\u00c5": "A",
    "\u00c7": "C",
    "\u00c8": "E",
    "\u00c9": "E",
    "\u00ca": "E",
    "\u00cb": "E",
    "\u00cc": "I",
    "\u00cd": "I",
    "\u00ce": "I",
    "\u00cf": "I",
    "\u00d1": "N",
    "\u00d2": "O",
    "\u00d3": "O",
    "\u00d4": "O",
    "\u00d5": "O",
    "\u00d6": "O",
    "\u00d8": "O",
    "\u00d9": "U",
    "\u00da": "U",
    "\u00db": "U",
    "\u00dc": "U",
    "\u00dd": "Y",
    "\u00e0": "a",
    "\u00e1": "a",
    "\u00e2": "a",
    "\u00e3": "a",
    "\u00e4": "a",
    "\u00e5": "a",
    "\u00e6": "ae",
    "\u00e7": "c",
    "\u00e8": "e",
    "\u00e9": "e",
    "\u00ea": "e",
    "\u00eb": "e",
    "\u00ec": "i",
    "\u00ed": "i",
    "\u00ee": "i",
    "\u00ef": "i",
    "\u00f1": "n",
    "\u00f2": "o",
    "\u00f3": "o",
    "\u00f4": "o",
    "\u00f5": "o",
    "\u00f6": "o",
    "\u00f8": "o",
    "\u00f9": "u",
    "\u00fa": "u",
    "\u00fb": "u",
    "\u00fc": "u",
    "\u00fd": "y",
    "\u00ff": "y",
    "\u0100": "A",
    "\u0101": "a",
    "\u0102": "A",
    "\u0103": "a",
    "\u0104": "A",
    "\u0105": "a",
    "\u0106": "C",
    "\u0107": "c",
    "\u010c": "C",
    "\u010d": "c",
    "\u010e": "D",
    "\u010f": "d",
    "\u0110": "D",
    "\u0111": "d",
    "\u0112": "E",
    "\u0113": "e",
    "\u0116": "E",
    "\u0117": "e",
    "\u0118": "E",
    "\u0119": "e",
    "\u011a": "E",
    "\u011b": "e",
    "\u011e": "G",
    "\u011f": "g",
    "\u0122": "G",
    "\u0123": "g",
    "\u0128": "I",
    "\u0129": "i",
    "\u012a": "I",
    "\u012b": "i",
    "\u012e": "I",
    "\u012f": "i",
    "\u0130": "I",
    "\u0131": "i",
    "\u0136": "K",
    "\u0137": "k",
    "\u0139": "L",
    "\u013a": "l",
    "\u013b": "L",
    "\u013c": "l",
    "\u013d": "L",
    "\u013e": "l",
    "\u0141": "L",
    "\u0142": "l",
    "\u0143": "N",
    "\u0144": "n",
    "\u0145": "N",
    "\u0146": "n",
    "\u0147": "N",
    "\u0148": "n",
    "\u014c": "O",
    "\u014d": "o",
    "\u0150": "O",
    "\u0151": "o",
    "\u0153": "oe",
    "\u0154": "R",
    "\u0155": "r",
    "\u0158": "R",
    "\u0159": "r",
    "\u015a": "S",
    "\u015b": "s",
    "\u015e": "S",
    "\u015f": "s",
    "\u0160": "S",
    "\u0161": "s",
    "\u0164": "T",
    "\u0165": "t",
    "\u0168": "U",
    "\u0169": "u",
    "\u016a": "U",
    "\u016b": "u",
    "\u016e": "U",
    "\u016f": "u",
    "\u0170": "U",
    "\u0171": "u",
    "\u0178": "Y",
    "\u0179": "Z",
    "\u017a": "z",
    "\u017b": "Z",
    "\u017c": "z",
    "\u017d": "Z",
    "\u017e": "z",
    "\u018f": "A",
    "\u01a0": "O",
    "\u01a1": "o",
    "\u01af": "U",
    "\u01b0": "u",
    "\u0218": "S",
    "\u0219": "s",
    "\u021a": "T",
    "\u021b": "t",
    "\u0259": "a",
    "\u0327": "",
    "\u0386": "\u0391",
    "\u0388": "\u0395",
    "\u0389": "\u0397",
    "\u038a": "\u0399",
    "\u038c": "\u039f",
    "\u038e": "\u03a5",
    "\u038f": "\u03a9",
    "\u0390": "\u03b9",
    "\u03aa": "\u0399",
    "\u03ab": "\u03a5",
    "\u03ac": "\u03b1",
    "\u03ad": "\u03b5",
    "\u03ae": "\u03b7",
    "\u03af": "\u03b9",
    "\u03b0": "\u03c5",
    "\u03ca": "\u03b9",
    "\u03cb": "\u03c5",
    "\u03cc": "\u03bf",
    "\u03cd": "\u03c5",
    "\u03ce": "\u03c9",
    "\u0401": "\u0415",
    "\u0407": "\u0406",
    "\u040e": "\u0423",
    "\u0419": "\u0418",
    "\u0439": "\u0438",
    "\u0451": "\u0435",
    "\u0457": "\u0456",
    "\u045e": "\u0443",
    "\u0622": "\u0627",
    "\u0623": "\u0627",
    "\u0624": "\u0648",
    "\u0625": "\u0627",
    "\u0626": "\u064a",
    "\u0649": "\u064a",
    "\u0671": "\u0627",
    "\u06c2": "\u06c1",
    "\u0b94": "\u0b92",
    "\u1e9e": "\u00df",
    "\u1ea0": "A",
    "\u1ea1": "a",
    "\u1ea2": "A",
    "\u1ea3": "a",
    "\u1ea4": "A",
    "\u1ea5": "a",
    "\u1ea6": "A",
    "\u1ea7": "a",
    "\u1ea8": "A",
    "\u1ea9": "a",
    "\u1eaa": "A",
    "\u1eab": "a",
    "\u1eac": "A",
    "\u1ead": "a",
    "\u1eae": "A",
    "\u1eaf": "a",
    "\u1eb0": "A",
    "\u1eb1": "a",
    "\u1eb2": "A",
    "\u1eb3": "a",
    "\u1eb4": "A",
    "\u1eb5": "a",
    "\u1eb6": "A",
    "\u1eb7": "a",
    "\u1eb8": "E",
    "\u1eb9": "e",
    "\u1eba": "E",
    "\u1ebb": "e",
    "\u1ebc": "E",
    "\u1ebd": "e",
    "\u1ebe": "E",
    "\u1ebf": "e",
    "\u1ec0": "E",
    "\u1ec1": "e",
    "\u1ec2": "E",
    "\u1ec3": "e",
    "\u1ec4": "E",
    "\u1ec5": "e",
    "\u1ec6": "E",
    "\u1ec7": "e",
    "\u1ec8": "I",
    "\u1ec9": "i",
    "\u1eca": "I",
    "\u1ecb": "i",
    "\u1ecc": "O",
    "\u1ecd": "o",
    "\u1ece": "O",
    "\u1ecf": "o",
    "\u1ed0": "O",
    "\u1ed1": "o",
    "\u1ed2": "O",
    "\u1ed3": "o",
    "\u1ed4": "O",
    "\u1ed5": "o",
    "\u1ed6": "O",
    "\u1ed7": "o",
    "\u1ed8": "O",
    "\u1ed9": "o",
    "\u1eda": "O",
    "\u1edb": "o",
    "\u1edc": "O",
    "\u1edd": "o",
    "\u1ede": "O",
    "\u1edf": "o",
    "\u1ee0": "O",
    "\u1ee1": "o",
    "\u1ee2": "O",
    "\u1ee3": "o",
    "\u1ee4": "U",
    "\u1ee5": "u",
    "\u1ee6": "U",
    "\u1ee7": "u",
    "\u1ee8": "U",
    "\u1ee9": "u",
    "\u1eea": "U",
    "\u1eeb": "u",
    "\u1eec": "U",
    "\u1eed": "u",
    "\u1eee": "U",
    "\u1eef": "u",
    "\u1ef0": "U",
    "\u1ef1": "u",
    "\u1ef2": "Y",
    "\u1ef3": "y",
    "\u1ef4": "Y",
    "\u1ef5": "y",
    "\u1ef6": "Y",
    "\u1ef7": "y",
    "\u1ef8": "Y",
    "\u1ef9": "y",
    "\u2e18": "",
}

# The capital dotted I: .NET ChangeCaseInvariant preserves it, so lowercasing skips it.
CAPITAL_DOTTED_I = "\u0130"


def remove_diacritics(text):
    """Replace each character the package's table lists by its base, after dropping U+FFFD."""
    return "".join(DIACRITICS.get(ch, ch) for ch in text if ch != "\ufffd")


def lower_invariant(text):
    """.NET invariant *simple* lowercasing: ICU's per-character mapping, capital dotted I preserved."""
    return "".join(
        ch if ch == CAPITAL_DOTTED_I else chr(icu.Char.tolower(ord(ch))) for ch in text
    )


def get_clean_value(value):
    """The clean key a value derives to: folded, lowercased, punctuation to single spaces, trimmed."""
    if not value.strip():
        return value  # empty or all whitespace is returned unchanged, as in Rust
    cleaned = lower_invariant(remove_diacritics(value))
    # [^\p{L}\p{N}\s] -> ' ', then collapse whitespace runs and trim. .NET matches UTF-16 units, so a
    # character outside the BMP (two surrogates) is never a letter or a number.
    spaced = "".join(
        ch
        if ord(ch) <= 0xFFFF and (unicodedata.category(ch)[0] in ("L", "N") or ch.isspace())
        else " "
        for ch in cleaned
    )
    return " ".join(spaced.split())


def repaired_clean_value(value):
    """The CleanValue the adoption repair derives for a Value, mirroring refresh_clean_values' `want`.

    The boot repair (``item_persistence_service.rs``) stores ``''`` for a whitespace-only Value and
    ``get_clean_value(Value)`` otherwise — unlike ``get_clean_value``, which returns a whitespace-only
    Value unchanged. The repair skips ``Value = ''`` and NULL rows, leaving their CleanValue as it was,
    so compare()'s unchanged-value clause decides them: the ``''`` returned for ``Value = ''`` and the
    ``None`` returned for a NULL Value are placeholders those rows are never actually judged against.
    """
    if value is None:
        return None
    return "" if not value.strip() else get_clean_value(value)

#!/usr/bin/env python3
"""A Python port of ``ferrofin_util::string_extensions::get_clean_value``.

The boot repair ``repair_clean_values`` (the port of Jellyfin 12.0's
``RefreshCleanNamesAsync``) rewrites every ``ItemValues.CleanValue`` to
``get_clean_value(Value)`` during adoption, so the table-digest check needs the
same derivation to tell a legitimate rewrite from damage. This mirrors the Rust
function (``crates/ferrofin-util/src/string_extensions.rs``) exactly: diacritic
removal with the same ligature folds and non-spacing-mark ranges, .NET invariant
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

# ligature_map(): the multi-character folds Jellyfin's Diacritics.Extensions applies.
# ð/þ are deliberately left alone (upstream does); ß folds to ss.
LIGATURES = {
    "œ": "oe", "Œ": "OE", "æ": "ae", "Æ": "AE",
    "ø": "o", "Ø": "O", "ł": "l", "Ł": "L", "ß": "ss",
}

# The capital dotted I: .NET ChangeCaseInvariant preserves it, so lowercasing skips it.
CAPITAL_DOTTED_I = "İ"


def _is_nonspacing_mark(cp):
    """The non-spacing-mark code-point ranges stripped after NFD, as the Rust port lists them."""
    return (
        0x0300 <= cp <= 0x036F  # Combining Diacritical Marks
        or 0x0483 <= cp <= 0x0489
        or 0x0591 <= cp <= 0x05BD or cp == 0x05BF or 0x05C1 <= cp <= 0x05C2
        or 0x05C4 <= cp <= 0x05C5 or cp == 0x05C7
        or 0x0610 <= cp <= 0x061A or 0x064B <= cp <= 0x065F or cp == 0x0670
        or 0x06D6 <= cp <= 0x06DC or 0x06DF <= cp <= 0x06E4
        or 0x06E7 <= cp <= 0x06E8 or 0x06EA <= cp <= 0x06ED
        or cp == 0x0711 or 0x0730 <= cp <= 0x074A
        or 0x1AB0 <= cp <= 0x1AFF
        or 0x1DC0 <= cp <= 0x1DFF  # Combining Diacritical Marks Supplement
        or 0x20D0 <= cp <= 0x20FF  # Combining Diacritical Marks for Symbols
    )


def remove_diacritics(text):
    """Fold ligatures and strip non-spacing marks, keeping precomposed non-accented forms (NFD→NFC)."""
    cleaned = text.replace("�", "")  # strip_non_conforming_unicode: drop U+FFFD (replacement char)
    out = []
    for ch in unicodedata.normalize("NFD", cleaned):
        if _is_nonspacing_mark(ord(ch)):
            continue
        out.append(LIGATURES.get(ch, ch))
    return unicodedata.normalize("NFC", "".join(out))


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
    # [^\p{L}\p{N}\s] -> ' ', then collapse whitespace runs and trim.
    spaced = "".join(
        ch if (unicodedata.category(ch)[0] in ("L", "N") or ch.isspace()) else " "
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

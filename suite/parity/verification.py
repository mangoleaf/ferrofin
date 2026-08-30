"""The closed set of parity VERIFICATION METHODS — how a ledger row was earned.

Every results row that carries a verdict (`deep_verified` true or false) must also
carry a `verification_method` from this set. `gen-ledger.py --check` rejects a row
with no method, an unknown method, or a method on a row that has no verdict; there
is no default, because a default is how a weak probe borrows a strong headline
(batch A2: a 15-boolean probe recorded `deep_verified` for rows whose bodies were
known not to diff clean).

Only ONE method is counted in the ledger's headline number, and the headline
sentence describes exactly that method and nothing else.

Reading the set, strongest first:

  body-diff     The response itself was compared against Jellyfin's and matched —
                every non-volatile field of the parsed body, or the bytes' sha256
                for a served file. THE HEADLINE.
  property      Named properties DERIVED from the response agreed on both servers:
                a decoded image's format and dimensions, a media type, a container
                signature, a normalised playlist, a set of named invariants. The
                bodies themselves were never compared — usually because they
                provably cannot be (two encoders, two random orders, two live
                transcodes).
  effect        A write was applied to both servers and its effect confirmed on
                each server's OWN read-back (after the favourite POST, that item's
                UserData.IsFavorite is true). No response body was diffed, and the
                two servers' responses were never compared with each other.
  status-class  The ONLY thing that agreed was the HTTP status class (plus, at
                most, a content-type family). Nothing was served, or the probe
                asked a deliberately-bogus id and both sides refused it. This is
                the weakest verdict in the ledger and it says so.
  empty-corpus  Both servers returned an EMPTY result set, so the only fields
                compared were the empty envelope's own zeros (TotalRecordCount,
                StartIndex). The handler's logic was not exercised: an
                unconditional-empty handler passes identically.

A row where NOTHING was compared is not a method — it is `deep_verified: null`
with no method, i.e. untested. `n == 0` from a diff means "no fields were
compared" just as often as "all fields matched", and only the first is honest.
"""

BODY_DIFF = "body-diff"
PROPERTY = "property"
EFFECT = "effect"
STATUS_CLASS = "status-class"
EMPTY_CORPUS = "empty-corpus"

#: The method the headline counts. Everything else is rendered separately.
HEADLINE = BODY_DIFF

#: name -> (glyph, short label, one-line meaning rendered in LEDGER.md)
METHODS = {
    BODY_DIFF: ("✅", "deep-verified",
                "the response (or a write's read-back) was itself diffed against "
                "Jellyfin 10.11.8 — every non-volatile field, or the served file's "
                "sha256 — and came back clean"),
    PROPERTY: ("◐", "property-verified",
               "named properties derived from the response agreed on both servers "
               "(decoded format/dimensions, media type, container signature, "
               "normalised playlist, named invariants); the bodies were NOT diffed"),
    EFFECT: ("⊙", "effect-verified",
             "a write was applied and its effect confirmed on each server's own "
             "read-back; no response body was diffed and the two servers' responses "
             "were never compared with each other"),
    STATUS_CLASS: ("▫", "status-class-only",
                   "only the HTTP status class agreed (at most also a content-type "
                   "family) — nothing was served, or a deliberately-bogus id was "
                   "refused by both"),
    EMPTY_CORPUS: ("∅", "empty-corpus",
                   "both servers returned an EMPTY result set, so only the empty "
                   "envelope's own zeros were compared — an unconditional-empty "
                   "handler would pass identically"),
}

VALID = frozenset(METHODS)


def is_empty_envelope(doc):
    """True for an empty QueryResult ENVELOPE: `Items: []` with a zero
    `TotalRecordCount`.

    Deliberately NARROW, and deliberately false for a bare `[]` — a QueryResult
    at least compares its own zeros, a bare list compares literally nothing, and
    collapsing the two would make `is_empty_envelope` mean "compared nothing"
    for one caller and "compared the zeros" for another. `is_empty_result`
    below is the union; it is the one `read_method` uses, and it says in its own
    name that it is the wider test.
    """
    return (isinstance(doc, dict) and doc.get("Items") == []
            and doc.get("TotalRecordCount") in (0, None))


def is_bare_empty_list(doc):
    """True for `[]` — the empty answer of the array-returning endpoints
    (`POST /Items/RemoteSearch/{kind}` answers `[]` when no fetcher matched).

    The most extreme empty there is: a diff of two of them performs ZERO leaf
    comparisons, so it can never be evidence of agreement about anything but
    the emptiness itself.
    """
    return isinstance(doc, list) and not doc


def is_empty_result(doc):
    """True for a response that carried no content, in EITHER wire shape.

    Diffing two of these proves only that both handlers answered empty, so the
    row is `empty-corpus` — never the headline, and never `deep_verified` on
    the strength of a leaf count that is zero.
    """
    return is_empty_envelope(doc) or is_bare_empty_list(doc)


def read_method(jbody, hbody, compared):
    """The honest method for a JSON read whose diff came back clean.

    `compared` is the number of non-volatile leaf comparisons the diff actually
    performed (`parity_diff.diff_stats`). Returns None when the probe produced no
    evidence at all — the caller must then record the row untested, not verified.

    The empty test comes FIRST because a bare `[] vs []` compares zero leaves:
    reporting it untested would hide that both servers were asked and both
    answered the same nothing, and reporting it `body-diff` would claim a
    comparison that never happened. `empty-corpus` says exactly what occurred,
    and gen-ledger keeps it out of the headline count.

    Note what this does NOT do: an empty answer on one side and content on the
    other is not an empty result, so it falls through to the leaf count and
    the caller's diff buckets — a one-sided empty is a divergence, never an
    agreement.
    """
    if is_empty_result(jbody) and is_empty_result(hbody):
        return EMPTY_CORPUS
    if not compared:
        return None
    return BODY_DIFF


def bare_status_class(sig):
    """True for a signature that carries nothing but a status class.

    Every Layer-3 signature helper falls back to `(status // 100, "")` when the
    server did not serve the thing — so two such signatures matching proves only
    that both refused. Used to DOWNGRADE a row's declared method to
    `status-class` from what the probe intended to compare, so a both-404 can
    never be rendered as "properties agreed".
    """
    return isinstance(sig, tuple) and len(sig) == 2 and sig[1] == ""


def selfcheck():
    assert HEADLINE in VALID and len(VALID) == 5
    assert is_empty_envelope({"Items": [], "TotalRecordCount": 0, "StartIndex": 0})
    assert not is_empty_envelope({"Items": [{"Name": "x"}], "TotalRecordCount": 1})
    # main's guard, kept verbatim: a bare list compares nothing at all, so it is
    # NOT an empty envelope. The array endpoints' empty answer is classified by
    # the separately-named `is_bare_empty_list`/`is_empty_result` pair, so the
    # widening is visible in the call site rather than hidden in this predicate.
    assert not is_empty_envelope([])
    assert is_bare_empty_list([]) and not is_bare_empty_list([{"Name": "x"}])
    assert is_empty_result([]) and is_empty_result({"Items": [], "TotalRecordCount": 0})
    assert not is_empty_result([{"Name": "x"}])
    assert read_method([], [], 0) == EMPTY_CORPUS
    # …but only when BOTH sides are empty. One side empty is a DIVERGENCE, so it
    # must not short-circuit to `empty-corpus`; it falls through to the leaf
    # count, which for a one-sided empty is zero — untested, not verified. Both
    # directions are asserted so the guard does not depend on which server
    # happened to be the empty one.
    assert read_method([], [{"Name": "x"}], 0) is None
    assert read_method([{"Name": "x"}], [], 0) is None
    # no evidence is untested, not verified
    assert read_method({}, {}, 0) is None
    assert read_method({"Items": [], "TotalRecordCount": 0}, {"Items": [], "TotalRecordCount": 0}, 2) \
        == EMPTY_CORPUS
    assert read_method({"A": 1}, {"A": 1}, 1) == BODY_DIFF
    assert bare_status_class((4, "")) and not bare_status_class((2, "image", "png", 1, 1))
    assert bare_status_class((2, ""))
    print(f"ok: {len(VALID)} verification methods, headline={HEADLINE!r}, "
          "empty-envelope / bare-empty-list + bare-status-class detectors")


if __name__ == "__main__":
    selfcheck()

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
  push-diff     The server→client WebSocket messages the operation CAUSED were
                captured on both servers, per receiving socket, and compared:
                the ordered sequence of message types that arrived (so "Jellyfin
                pushed two, Ferrofin pushed one" is a red, not a pass), every
                non-volatile field of each message's payload, and a command's
                scheduling instant through the derived offset
                `When − EmittedAt` (the two wall clocks themselves cannot cross
                instances) — and, where the op returns a body, that body diffed
                clean too. It says nothing about sockets the probe did not open,
                and nothing about delivery timing beyond the probe's bounded
                quiet window. Counted and listed separately; it is NOT the
                headline, because no claim about the HTTP response alone is
                being made.
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
                unconditional-empty handler passes identically. "Empty" is
                judged over the WHOLE document, at any nesting depth and under
                any key: `ThemeMedia` wraps three empty `QueryResult`s in one
                object, and inspecting only the top level let it reach the
                headline having compared six zeros.

A row where NOTHING was compared is not a method — it is `deep_verified: null`
with no method, i.e. untested. `n == 0` from a diff means "no fields were
compared" just as often as "all fields matched", and only the first is honest.
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import parity_diff  # noqa: E402  — the same VOLATILE denylist the diff walks

BODY_DIFF = "body-diff"
PUSH_DIFF = "push-diff"
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
    PUSH_DIFF: ("⇄", "push-verified",
                "the server→client WebSocket messages the op caused were captured "
                "on BOTH servers and diffed — the ordered sequence of message "
                "types per receiving socket, every non-volatile field of each "
                "message's payload, and a command's scheduling instant via the "
                "derived `When − EmittedAt` offset, plus the HTTP response body "
                "where there is one; it asserts nothing about sockets the probe "
                "never opened, nor about timing beyond its bounded quiet window"),
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


#: Bookkeeping keys of a result envelope. When the corresponding result set is
#: EMPTY these carry no information — `TotalRecordCount: 0` next to `Items: []`
#: is the envelope restating its own emptiness, not a field of any content.
#: A non-zero value IS content (it says the corpus is not empty), so only the
#: falsy case is discounted.
ENVELOPE_KEYS = frozenset({"TotalRecordCount", "StartIndex", "TotalCount", "Limit"})


def _corpus_scan(doc, key=None, volatile=None):
    """`(content_leaves, empty_lists)` over exactly what a parity diff would walk.

    Mirrors `parity_diff.diff`'s traversal: VOLATILE keys are skipped, dicts and
    lists recurse, scalars are leaves. A leaf counts as CONTENT unless it is an
    empty envelope's own zero (see `ENVELOPE_KEYS`). An empty list is not content
    — it is the absence of it — but it is counted separately, because a document
    with no empty list in it was never an "empty result set" to begin with.
    """
    if volatile is None:
        volatile = parity_diff.VOLATILE
    if isinstance(doc, dict):
        content = empties = 0
        for k, v in doc.items():
            if volatile.match(k):
                continue
            c, e = _corpus_scan(v, k, volatile)
            content += c
            empties += e
        return content, empties
    if isinstance(doc, list):
        if not doc:
            return 0, 1
        content = empties = 0
        for v in doc:
            c, e = _corpus_scan(v, None, volatile)
            content += c
            empties += e
        return content, empties
    if key in ENVELOPE_KEYS and not doc:
        return 0, 0
    return 1, 0


def is_empty_envelope(doc):
    """True when a document carried NO content — only empty result sets and their zeros.

    Not a top-level shape test. The document is scanned the way the diff walks it,
    at every depth and under every key, and the answer is yes only when it contains
    at least one empty list and not one leaf of actual content. That covers a bare
    `{"Items": [], "TotalRecordCount": 0}`, a differently-keyed envelope
    (`SearchHints`, `Groups`), and a WRAPPER around several of them —
    `GET /Items/{itemId}/ThemeMedia` returns `{ThemeSongsResult: {...}, ...}` and
    reached the body-diff headline having compared six nested zeros, because the
    old test only ever looked at `doc["Items"]`.

    Diffing two of these compares the envelopes' own bookkeeping and no content at
    all, so the row is `empty-corpus`, never the headline.
    """
    if not isinstance(doc, (dict, list)):
        return False
    content, empties = _corpus_scan(doc)
    return empties > 0 and content == 0


def read_method(jbody, hbody, compared):
    """The honest method for a JSON read whose diff came back clean.

    `compared` is the number of non-volatile leaf comparisons the diff actually
    performed (`parity_diff.diff_stats`). Returns None when the probe produced no
    evidence at all — the caller must then record the row untested, not verified.
    """
    if not compared:
        return None
    if is_empty_envelope(jbody) and is_empty_envelope(hbody):
        return EMPTY_CORPUS
    return BODY_DIFF


def bare_status_class(sig):
    """True for a signature that establishes nothing beyond the HTTP status class.

    Every Layer-3 signature is built to one convention:

        (status_class, kind, *evidence)

    `kind` is a LABEL — a content-type family (`"image"`, `"audio/flac"`,
    `"css"`), or the empty string the helpers fall back to when the server served
    nothing. A label is a header, not proof that anything was served or decoded.
    `evidence` is what the probe actually measured: a decoded image's format and
    dimensions, a sha256, an ffprobe result, a normalised playlist, `bool(body)`.

    So a row is status-class-only when it has NO evidence — either the tuple stops
    at the label (every HEAD signature: `(2, "audio/flac")` says the status class
    agreed and the media type agreed, which is exactly what the closed set defines
    as `status-class`), or every evidence element is falsy (`(2, "css", False)` —
    a 200 with an EMPTY body; `(2, "none", False)` — the deliberately-nonexistent
    fallback font; `(4, "")` — both servers refused).

    Used to DOWNGRADE a row's declared method, so a both-404, a headers-only HEAD
    and an empty body can never be rendered as "properties agreed". The old test
    was `len == 2 and sig[1] == ""`, a string comparison that saw only the last of
    those three.
    """
    if not isinstance(sig, (tuple, list)) or not sig:
        return False
    return not any(sig[2:])


def selfcheck():
    assert HEADLINE in VALID and len(VALID) == 6
    # push-diff is a SIXTH method, added deliberately (batch E1) rather than
    # letting a pushed-message differential borrow the body-diff headline: no
    # HTTP response body is what makes the claim, so it may not be counted as if
    # one were. It must never become the headline.
    assert PUSH_DIFF in VALID and PUSH_DIFF != HEADLINE

    # --- empty corpora, at every depth and under every key -------------------
    assert is_empty_envelope({"Items": [], "TotalRecordCount": 0, "StartIndex": 0})
    assert is_empty_envelope({"SearchHints": [], "TotalRecordCount": 0})
    # The regression this test exists for: three empty QueryResults in a wrapper.
    # `OwnerId` is VOLATILE, so the six zeros below were the ENTIRE body diff of
    # GET /Items/{itemId}/ThemeMedia, and it was counted as deep-verified.
    theme = {"OwnerId": "abc",
             "ThemeSongsResult": {"Items": [], "TotalRecordCount": 0, "StartIndex": 0},
             "ThemeVideosResult": {"Items": [], "TotalRecordCount": 0, "StartIndex": 0},
             "SoundtrackSongsResult": {"Items": [], "TotalRecordCount": 0, "StartIndex": 0}}
    assert is_empty_envelope(theme), "a nested/wrapped empty envelope must not reach the headline"
    assert read_method(theme, dict(theme), 6) == EMPTY_CORPUS

    # ...and what must NOT be mistaken for one: any leaf of real content.
    assert not is_empty_envelope({"Items": [{"Name": "x"}], "TotalRecordCount": 1})
    assert not is_empty_envelope({"Version": "10.11.8"})          # no empty list at all
    assert not is_empty_envelope({"Items": [], "Enabled": True})  # a real scalar alongside
    assert not is_empty_envelope({"Items": [], "TotalRecordCount": 3})  # count contradicts
    assert not is_empty_envelope({"A": {"Items": [], "TotalRecordCount": 0}, "B": 7})
    # A bare `[]` carries nothing AND compares nothing: read_method calls that
    # untested (no method) before the empty-corpus question is ever asked.
    assert read_method([], [], 0) is None

    # no evidence is untested, not verified
    assert read_method({}, {}, 0) is None
    assert read_method({"Items": [], "TotalRecordCount": 0}, {"Items": [], "TotalRecordCount": 0}, 2) \
        == EMPTY_CORPUS
    assert read_method({"A": 1}, {"A": 1}, 1) == BODY_DIFF

    # --- signatures: (status_class, kind_label, *evidence) -------------------
    # nothing served / nothing measured -> status-class only
    assert bare_status_class((4, ""))
    assert bare_status_class((2, ""))
    assert bare_status_class((2, "audio/flac"))     # a HEAD: status + media type, no body
    assert bare_status_class((2, "image"))          # a HEAD mirror of an image GET
    assert bare_status_class((2, "css", False))     # 200 with an EMPTY body
    assert bare_status_class((2, "none", False))    # the nonexistent fallback font
    assert not bare_status_class(())          # not a signature at all
    # real evidence -> keeps whatever the layer declared
    assert not bare_status_class((2, "image", "png", 600, 600))
    assert not bare_status_class((2, "text/plain", True))          # a log: type + non-empty
    assert not bare_status_class((2, "file", "deadbeef", "video/x-matroska", "bytes"))
    assert not bare_status_class((2, "text/vtt", "WEBVTT\n\n00:00.000 --> 00:01.000\nhi"))
    assert not bare_status_class((2, "m3u8", "application/x-mpegurl", ("#EXTM3U",)))

    print(f"ok: {len(VALID)} verification methods, headline={HEADLINE!r}, "
          "nested-aware empty-envelope + evidence-based status-class detectors")


if __name__ == "__main__":
    selfcheck()

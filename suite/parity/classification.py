"""The closed set of parity CLASSIFICATION CATEGORIES — what a divergence IS.

A ledger row can carry a curated classification (`parity/classifications.json`).
That text is prose, written for a human. Its `category` is the machine-readable
half: it decides which SECTION of LEDGER.md the row renders in and which tally
it is counted in.

Before this module existed, the section was chosen by string-prefixing the
PROSE — a classification starting with the literal "open-work" went to the open
section, one starting with "flagged" to the unreviewed section, and EVERYTHING
ELSE fell into "Classified divergence (accepted — not a bug)". That default is
how a row lands in the accepted bucket by saying nothing in particular. Batch
D3's `POST /LiveTv/Programs` row is the case that exposed it: its own text says
"NOT a Ferrofin divergence, and NOT an accepted one", it is currently RED with a
live 34-key diff, and it rendered under "accepted — not a bug" and was counted
there. The `category` field it carried ("lab-state") was read by nothing.

So: every curated row declares a category from this set, `gen-ledger.py --check`
rejects one that does not, and the category — never the prose — picks the
bucket. There is no default bucket.

Reading the buckets, strongest claim first:

  accepted   A settled decision. Ferrofin differs from Jellyfin and that is
             correct (or deliberately tolerated). The question is CLOSED.
  note       A recorded fact about a row that carries no divergence of its own —
             either its verdict is earned somewhere else (a push probe, a
             journey, an obsolete-endpoint citation), or no body-level verdict is
             reachable for it at all and the row says so. Not a divergence, and
             not evidence on its own.
  lab-state  The two servers' DATA differ, not their code. The op is right and
             the diff is real; it is an artefact of a long-lived shared lab.
             NOT accepted: on a freshly-wiped pair the row must come back clean,
             and if it does not it is a bug.
  no-probe   Nothing has exercised this op yet — a missing probe, or one that
             needs credentials/hardware the lab does not have. An ABSENCE of
             evidence. NOT accepted, and emphatically not verified.
  open-work  A named, real gap with an owner and an un-defer path. NOT
             accepted: rendering an unfinished port under "accepted" is how a
             gap quietly becomes a decision.
  flagged    A detector wrote this text and nobody has read it. Neither an
             accepted divergence nor a named work item.
"""

ACCEPTED = "accepted"
NOTE = "note"
LAB_STATE = "lab-state"
NO_PROBE = "no-probe"
OPEN_WORK = "open-work"
FLAGGED = "flagged"

#: bucket -> (glyph, LEDGER.md heading, the sentence rendered under it)
BUCKETS = {
    ACCEPTED: ("⚠️", "Classified divergence (accepted — not a bug)",
               "Settled decisions. An item here is a question that has been closed."),
    NOTE: ("📝", "Recorded note (NOT a divergence, NOT a verdict)",
           "A fact worth keeping next to an op whose verdict is earned elsewhere — "
           "a push probe, a write journey, an obsolete-endpoint citation — or next "
           "to one for which no body-level verdict is reachable, where the note "
           "records what WAS measured and what would verify the rest. It claims "
           "nothing about this row on its own."),
    LAB_STATE: ("🧪", "Lab state (NOT accepted — the servers' DATA differ, not their code)",
                "The diff is real and is NOT dropped from the compare. Its cause was "
                "measured, and it is drift in a long-lived shared lab rather than a "
                "Ferrofin behaviour. On the authoritative freshly-wiped pair these must "
                "come back clean; one that does not is a bug, not an accepted state."),
    NO_PROBE: ("🕳", "No probe yet (NOT accepted, NOT verified)",
               "An absence of evidence: no layer exercises this op, or the one that "
               "would needs credentials or a device the lab does not have. These sat "
               "under \"accepted — not a bug\" until this section existed, which read "
               "an untested op as a closed question."),
    OPEN_WORK: ("⛏", "Open work (NOT accepted — a named divergence still to port)",
                "Real gaps with an owner and a path, kept OUT of the accepted section: "
                "rendering an unfinished port under \"accepted\" is how a gap quietly "
                "becomes a decision."),
    FLAGGED: ("🔍", "Flagged by a detector — UNREVIEWED (not accepted, not verified)",
              "A probe recorded a divergence and wrote its own text. Nobody has looked "
              "at these yet: they are neither an accepted divergence nor a named work "
              "item."),
}

#: The curated categories, and the bucket each renders in. Adding a category is
#: adding a line here — a category outside this map fails `gen-ledger.py --check`
#: rather than silently defaulting into the accepted tally.
CATEGORIES = {
    # --- settled decisions -------------------------------------------------
    "accepted-divergence": ACCEPTED,
    "expected-extension": ACCEPTED,
    "instance": ACCEPTED,
    "jellyfin-bug": ACCEPTED,
    # --- notes on rows verified elsewhere ----------------------------------
    "verified": NOTE,
    "verified-by-push-probe": NOTE,
    # The row's BODY cannot be compared across two instances — each server's own
    # repository catalogue, its own plugin set — and its evidence is a named
    # property probe instead. The row's `verification_method` already says
    # `property`; this says the same thing about the CLASSIFICATION, so the text
    # is read as "here is what WAS measured", never as a divergence.
    "property": NOTE,
    # A cross-user authorization hole that was FOUND and CLOSED. Not a
    # divergence (the two servers agree now) and not a verdict (the row's own
    # probe still has to earn one) — a recorded fact about how the row got here,
    # kept because deleting the only record of a real defect is how the defect
    # comes back.
    "security-fixed": NOTE,
    # The breadth sweep records `deep_verified` ONLY for a 200/200 pair (sweep.py:
    # the diff lives inside `hs == 200 and js == 200`). An op that answers the same
    # NON-2xx on both servers is therefore measured — its statuses agree, its row
    # carries `status_conformant: true` — and yet carries no verdict at all, because
    # there is no body anywhere to diff. `verified` was the only NOTE category
    # available for such a row and its very name reads as the verdict the row does
    # not have; this one says what is true instead.
    "status-class-only": NOTE,
    # --- not accepted ------------------------------------------------------
    "lab-state": LAB_STATE,
    "remote-opt-in": NO_PROBE,
    "requires-channel-plugin": NO_PROBE,
    "requires-livetv-tuner": NO_PROBE,
    "open-work": OPEN_WORK,
}

VALID = frozenset(CATEGORIES)

#: The one bucket whose heading claims a human closed the question.
SETTLED = ACCEPTED


def bucket(category, classification):
    """The bucket a ledger row renders in, or `None` when it carries no
    classification at all.

    `category` comes from the curated overlay and wins outright. A row with no
    category can only have been written by a live layer, whose vocabulary is
    exactly `""`, `"ok"` and `"flagged: …"` — so a detector flag is the only
    thing left to recognise, and everything else is simply unclassified.
    """
    if category:
        return CATEGORIES.get(category)
    if (classification or "").startswith("flagged"):
        return FLAGGED
    return None


def selfcheck():
    assert set(CATEGORIES.values()) <= set(BUCKETS)
    assert SETTLED in BUCKETS
    # Every bucket must be reachable, or its heading is a promise nothing keeps.
    unreachable = set(BUCKETS) - set(CATEGORIES.values()) - {FLAGGED}
    assert not unreachable, f"bucket with no category: {unreachable}"
    assert bucket("lab-state", "") == LAB_STATE
    assert bucket("open-work", "") == OPEN_WORK
    assert bucket("requires-livetv-tuner", "") == NO_PROBE
    assert bucket("instance", "") == ACCEPTED
    # A status-only row is a NOTE, never a verdict and never an accepted divergence.
    assert bucket("status-class-only", "") == NOTE
    assert CATEGORIES["status-class-only"] != SETTLED
    # Same for the two categories that record what a row's own probe measured or
    # repaired: neither may be counted as a closed question.
    assert bucket("property", "") == NOTE and bucket("security-fixed", "") == NOTE
    assert SETTLED not in (CATEGORIES["property"], CATEGORIES["security-fixed"])
    # The defect this module exists to prevent: an unknown category must NOT
    # fall through to "accepted". It buckets nowhere, and `check()` rejects it.
    assert bucket("something-new", "") is None
    # No category: only a detector flag is recognised.
    assert bucket(None, "flagged: verify") == FLAGGED
    assert bucket(None, "ok") is None and bucket(None, "") is None
    # A curated category wins over the auto text it overrode.
    assert bucket("instance", "flagged: verify") == ACCEPTED
    print(f"ok: {len(CATEGORIES)} categories over {len(BUCKETS)} buckets, "
          f"settled={SETTLED!r}, no default bucket")


if __name__ == "__main__":
    selfcheck()

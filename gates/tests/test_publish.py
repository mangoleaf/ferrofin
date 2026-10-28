"""gates/publish.py: the release summary from fixed benchmark results, and the README Results block and its
graphs from fixed summary files (stdlib)."""

import contextlib
import copy
import re
import subprocess
import io
import json
import os
import sys
import tempfile
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path
from unittest import mock

GATES = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(GATES))
import publish  # noqa: E402
import result  # noqa: E402
import session  # noqa: E402

sys.path.insert(0, str(GATES / "adoption"))
import adoption_report  # noqa: E402
sys.path.insert(0, str(GATES / "parity"))
import diff as parity_diff  # noqa: E402
import importlib.util  # noqa: E402
_spec = importlib.util.spec_from_file_location("gates_bench_gate_for_tests", GATES / "bench" / "gate.py")
sys.path.insert(0, str(GATES / "bench"))
bench_gate = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(bench_gate)

README = "# Ferrofin\n\nIntro.\n\n## Results\n\n<!-- BEGIN GENERATED RESULTS -->\nold\n<!-- END GENERATED RESULTS -->\n\n## After\n"


def cell(median, unit="ms", flag=None):
    return {"median": median, "min": median, "max": median, "values": [median], "unit": unit, "flag": flag}


def row(name, ferrofin, jellyfin, unit="ms", withheld=None):
    ratio = None if withheld else round(jellyfin / ferrofin, 4)
    return {"name": name, "servers": {"jellyfin": cell(jellyfin, unit), "ferrofin": cell(ferrofin, unit)},
            "ratio": ratio, "withheld": withheld}


def bench_result(scale=1.0, digest="sha256:img", search_withheld=None, started=None):
    """A benchmark run's result.json as gates/bench/report.py writes it, its Ferrofin numbers times `scale`."""
    level = lambda: {"rate": 5, "screens": [row("home", 17 * scale, 55), row("movies", 17 * scale, 43),
                                            row("search", 30 * scale, 64, withheld=search_withheld)],
                     "endpoints": [row("home:image", 0.74 * scale, 5.4), row("playback:intros", 6.0 * scale, 5.4)]}
    detail = {"runs": ["r"], "servers": {"jellyfin": "Jellyfin 12.2.0", "ferrofin": "Ferrofin"},
              "oracle_label": "Jellyfin 12.2.0", "conditions": {}, "levels": {"loaded": level(), "stress": level()},
              "ttfs": [row("cold start (restart → authenticated views; host caches retained)", 379 * scale, 3420),
                       row("HLS first segment (forced transcode)", 203 * scale, 434)],
              "memory": [row("peak under load", 147 * scale, 470, "MiB"), row("steady idle", 106 * scale, 295, "MiB")],
              "cpu_per_screen": [row("loaded", 33 * scale, 132, withheld="Ferrofin: search differs")],
              "missing": [], "load_problems": []}
    source = {"commit": "5112240e", "image": "ferrofin:x", "image_digest": digest, "oracle": "jellyfin/jellyfin@sha256:pin",
              "host_class": {"cores": 32, "memory_gib": 125}, "fixture": "4b217db66afc0000"}
    started = started or f"2026-10-26T01:{int(scale * 100) % 60:02d}:00Z"   # each run its own start
    return result.envelope("bench", "measured", {"completed": 76}, source, started, None, detail)


def parity_requests():
    """A parity run's requests as diff.py records them: two areas, each outcome at least once."""
    new = lambda path, filed=None, ledger=None: {"path": path, "kind": "value", "ledger": ledger, "filed": filed}
    return ([{"target": "/Items/1", "state": "match", "differences": []}] * 6
            + [{"target": "/Items/2?Fields=x", "state": "ledgered", "differences": [new("A", ledger="ledger.toml#1")]}] * 3
            + [{"target": "/Sessions", "state": "differences", "differences": [new("B")]},
               {"target": "/Videos/1", "state": "differences", "differences": [new("C", filed=832)]},
               {"target": "/Intros", "state": "oracle failed", "differences": []}])


def adoption_session(root, rows, commit="5112240e1923cdc9", dirty=False, parity=True):
    """A gates session whose adoption gate recorded `rows` ([(fixture, kind, expected, verdict, detail)]) and wrote
    its result.json, and (with `parity`) whose parity run wrote its own, as gates/run, gates/adoption/run.sh and
    gates/parity/diff.py leave them."""
    from datetime import datetime
    s = Path(session.new(str(root / "sessions"), ["adoption"], "release/v1.4.0", commit, dirty, "ferrofin:x",
                         "sha256:img", "jellyfin/jellyfin@sha256:pin", when=datetime(2026, 10, 26, 1, 0)))
    recorder = adoption_report.Recorder(s / "adoption")
    recorder.start("gates/adoption/run.sh", image="ferrofin:x", oracle="12.2")
    for fixture, kind, expected, verdict, detail in rows:
        recorder.row(fixture, kind, expected, verdict, detail)
    adoption_report.gate_result().write(str(s / "adoption"), adoption_report.result_doc(*adoption_report.load(s / "adoption")))
    if parity:
        summary = {"verdict": "new differences", "reason": "1 new difference(s) in 2 request(s)", "planned": 12,
                   "requests": parity_requests(), "run": {"oracle": {"version": "12.2.0", "image": "jellyfin@pin"}, "sha": commit[:8]}}
        (s / "parity").mkdir()
        parity_diff.gate_result().write(str(s / "parity"), parity_diff.result_doc(summary))
    return s


ADOPTION = [("jellyfin-10.11.8", "path", "10.11.8", "PASS", ""), ("jellyfin-12.2-from-10", "path", "12.2.0", "PASS", ""),
            ("jellyfin-10.11.11-messy", "messy", "10.11.11", "FAIL", "messy: gone_file_userdata " + "x" * 400),
            ("jellyfin-12.1-from-12", "path", "12.1.0", "SKIP", "fixture not built")]


def ab_session(root, verdicts, mode="ab", blocking=()):
    """A gates session run with PREVIOUS=: its bench/ folder is a gate.py session whose result.json gate.py's own
    session_result wrote (three pairs)."""
    s = root / "ab-session"
    (s / "bench").mkdir(parents=True)
    (s / "session.json").write_text("{}")
    plan = {"mode": mode, "name": "bench", "date": "2026-10-26T03:00:00+00:00",
            "schedule": [[f"0{i}-{r}", r, img] for i, (r, img) in enumerate([("a", "v1.3.2"), ("b", "rc")] * 3, 1)],
            "images": {"a": {"ref": "ghcr.io/mangoleaf/ferrofin:v1.3.2", "id": "sha256:a"},
                       "b": {"ref": "ferrofin:v1.4.0-rc", "id": "sha256:b"}}}
    (s / "bench" / "02-b").mkdir()   # the candidate's first run, whose run.json names the commit
    (s / "bench" / "02-b" / "run.json").write_text(json.dumps({"sha": "5112240e", "ids_sha256": "4b21"}))
    result.write(str(s / "bench"), bench_gate.session_result(str(s / "bench"), plan, {"verdicts": verdicts}, list(blocking)))
    return s


AB_VERDICTS = {
    "loaded/screens/home": {"kind": "screens", "unit": "ms", "a_median": 20, "b_median": 10, "pct": -50.0, "verdict": "improved", "gated": True},
    "loaded/screens/movies": {"kind": "screens", "unit": "ms", "a_median": 10, "b_median": 10.2, "pct": 2.0, "verdict": "within noise", "gated": True},
    "memory/peak under load": {"kind": "memory", "unit": "MiB", "a_median": 100, "b_median": 125, "pct": 25.0, "verdict": "regressed", "gated": True},
    "loaded/endpoints/items": {"kind": "endpoints", "unit": "ms", "a_median": 4, "b_median": 3, "pct": -25.0, "verdict": "improved", "gated": True},
    "loaded/screens/search": {"kind": "screens", "unit": "ms", "verdict": "not comparable", "reason": "other work", "gated": True},
    "loaded/endpoints/x": {"kind": "endpoints", "unit": "ms", "a_median": 1, "b_median": 1, "verdict": "within noise", "gated": False}}


class PublishTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)

    def runs(self, docs):
        paths = []
        for doc in docs:
            d = self.root / "runs" / f"bench-{len(list((self.root / 'runs').glob('bench-*'))) if (self.root / 'runs').exists() else 0}"
            d.mkdir(parents=True)
            result.write(str(d), doc)
            paths.append(str(d))
        return paths

    def summary(self, scales=(1.0, 1.1, 0.9), **kw):
        return publish.summarise("v1.4.0", self.runs([bench_result(s, started=f"2026-10-26T0{i}:00:00Z", **kw)
                                                      for i, s in enumerate(scales)]))

    def published(self, summaries):
        results = self.root / "docs" / "results"
        for doc in summaries:
            publish.write_json(str(results / doc["version"] / "summary.json"), doc)
        return results

    def test_the_summary_is_the_median_of_the_runs_with_its_source(self):
        doc = self.summary()
        self.assertEqual(publish.summary_problems(doc), [])
        bench = doc["bench"]
        home = bench["levels"]["loaded"]["screens"][0]
        self.assertEqual((home["ferrofin"], home["jellyfin"]["median"]), ({"median": 17.0, "min": 15.3, "max": 18.7}, 55))
        self.assertEqual(home["ratio"], round(55 / 17, 4))   # whole milliseconds on both sides, as the reports print a screen
        intros = bench["levels"]["loaded"]["endpoints"][1]
        self.assertLess(intros["ratio"], 1)   # a slower cell stays in
        self.assertEqual(bench["source"]["run_count"], 3)
        self.assertEqual(bench["source"]["runs"], ["bench-0", "bench-1", "bench-2"])
        self.assertEqual((bench["source"]["oracle"], bench["source"]["date"], bench["source"]["host_class"]),
                         ("Jellyfin 12.2.0", "2026-10-26", {"cores": 32, "memory_gib": 125}))
        self.assertEqual([r["name"] for r in bench["memory"]], ["peak under load", "steady idle"])

    def test_a_cell_any_run_withholds_or_lacks_is_withheld_with_every_reason(self):
        docs = [bench_result(1.0, started=f"2026-10-26T0{i}:00:00Z") for i in range(3)]
        docs[1] = bench_result(1.0, search_withheld="Ferrofin: ids differ", started="2026-10-26T01:00:00Z")
        del docs[2]["detail"]["levels"]["loaded"]["endpoints"][0]
        bench = publish.summarise("v1.4.0", self.runs(docs))["bench"]
        search = bench["levels"]["loaded"]["screens"][2]
        self.assertEqual((search["ratio"], search["withheld"]), (None, "Ferrofin: ids differ"))
        image = bench["levels"]["loaded"]["endpoints"][0]
        self.assertEqual((image["ratio"], image["withheld"]), (None, "missing from run 3"))

    def test_a_row_any_run_has_is_published_and_withheld_where_the_others_lack_it(self):
        docs = [bench_result(1.0, started=f"2026-10-26T0{i}:00:00Z") for i in range(3)]
        docs[1]["detail"]["levels"]["loaded"]["endpoints"].append(row("extra:only-in-run-2", 1, 2))
        del docs[2]["detail"]["levels"]["stress"]
        docs[0]["detail"]["memory"][0]["servers"]["ferrofin"]["median"] = None   # a value one run lacks
        bench = publish.summarise("v1.4.0", self.runs(docs))["bench"]
        extra = bench["levels"]["loaded"]["endpoints"][-1]
        self.assertEqual((extra["name"], extra["ratio"], extra["withheld"]), ("extra:only-in-run-2", None, "missing from run 1, 3"))
        self.assertEqual({r["withheld"] for r in bench["levels"]["stress"]["screens"]}, {"missing from run 3"})
        self.assertEqual((bench["memory"][0]["ratio"], bench["memory"][0]["withheld"]), (None, "ferrofin has no value in every run"))

    def test_summarise_refuses_a_run_with_no_source_or_another_commit(self):
        for key, value, refusal in (("fixture", None, "a run records no fixture"), ("commit", "", "a run records no commit"),
                                    ("commit", "other", "the runs differ in commit")):
            with self.subTest(key=key, value=value):
                docs = [bench_result(s) for s in (1.0, 1.1, 0.9)]
                docs[2]["source"][key] = value
                with self.assertRaisesRegex(publish.Refused, refusal):
                    publish.bench_summary(docs, ["a", "b", "c"])

    def test_summarise_refuses_what_cannot_be_published(self):
        for docs, refusal in (([bench_result(1.0), bench_result(1.1)], "2 benchmark runs: publishable numbers are the median of at least 3"),
                              ([bench_result(1.0), bench_result(1.1), bench_result(0.9, digest="sha256:other")], "the runs differ in image_digest"),
                              ([bench_result(1.0), bench_result(1.0), bench_result(1.1)], "every run needs its own start time")):
            with self.subTest(refusal), self.assertRaisesRegex(publish.Refused, refusal):
                publish.summarise("v1.4.0", self.runs(docs))
            for d in (self.root / "runs").iterdir():
                for f in d.iterdir():
                    f.unlink()
                d.rmdir()
        ferrofin_only = bench_result()
        del ferrofin_only["detail"]["servers"]["jellyfin"]
        with self.assertRaisesRegex(publish.Refused, "did not measure Jellyfin"):
            publish.summarise("v1.4.0", self.runs([ferrofin_only]))
        one = self.runs([bench_result(1.0)])
        with self.assertRaisesRegex(publish.Refused, "a run is named twice"):
            publish.summarise("v1.4.0", one * 3)
        with self.assertRaisesRegex(publish.Refused, "has no result.json"):
            publish.summarise("v1.4.0", [str(self.root)])
        with self.assertRaisesRegex(publish.Refused, "not a release version"):
            publish.summarise("1.4", [])

    def test_summary_problems_name_what_is_wrong(self):
        doc = self.summary()
        self.assertEqual(publish.summary_problems({**doc, "schema": 2})[0], "schema is 2, not 1")
        doc["parity"] = {"source": {"session": "s", "commit": "c", "oracle": "Jellyfin 12.2.0", "date": "d"}, "sent": 2,
                         "counts": {"identical": 2}, "areas": [{"area": "/Items", "requests": 2, "identical": 2}]}
        self.assertEqual(publish.summary_problems(doc), [])
        broken = copy.deepcopy(doc)
        broken["parity"]["areas"][0]["identical"] = 1
        self.assertTrue(any("parity area is not shaped" in p for p in publish.summary_problems(broken)))
        del doc["parity"]
        for change, problem in ((lambda d: d["bench"].pop("ttfs"), "bench.ttfs is not a list"),
                                (lambda d: d["bench"]["memory"][0].update(ratio=0), "has a ratio but not two positive medians"),
                                (lambda d: d["bench"]["memory"][0].update(ratio=None), "neither a ratio nor a reason"),
                                (lambda d: d["bench"].update(levels=[]), "bench.levels is not an object of levels")):
            broken = copy.deepcopy(doc)
            change(broken)
            with self.subTest(problem):
                self.assertTrue(any(problem in p for p in publish.summary_problems(broken)), publish.summary_problems(broken))
        broken = copy.deepcopy(doc)
        broken["bench"]["source"]["run_count"] = 1
        broken["bench"]["ttfs"][0]["withheld"] = "x"
        self.assertEqual(publish.summary_problems(broken), ["bench.source.run_count is under 3",
                                                            f"bench row {broken['bench']['ttfs'][0]['name']} has a ratio and is withheld"])

    def test_the_readme_block_and_graphs_render_byte_identically_from_the_summaries(self):
        results = self.published([self.summary()])
        files, readme = publish.render(str(results), README, str(self.root))
        again, readme_again = publish.render(str(results), README, str(self.root))
        self.assertEqual((files, readme), (again, readme_again))
        stems = {Path(p).name for p in files}
        for stem in ("screens-loaded", "screens-stress", "endpoints-loaded", "endpoints-stress", "startup", "memory"):
            self.assertTrue({f"{stem}.svg", f"{stem}-dark.svg"} <= stems, stem)
        self.assertNotIn("cpu.svg", stems)   # its only row is withheld: no graph, the reason listed instead
        self.assertTrue(readme.startswith("# Ferrofin\n\nIntro.\n\n## Results\n\n<!-- BEGIN GENERATED RESULTS"))
        self.assertTrue(readme.endswith("<!-- END GENERATED RESULTS -->\n\n## After\n"))
        for text in ("**9.0× faster cold start**", "**3.2× lighter peak memory under load**",
                     '<source media="(prefers-color-scheme: dark)" srcset="docs/results/v1.4.0/screens-loaded-dark.svg">',
                     'src="docs/results/v1.4.0/screens-loaded.svg"', "| home | 17 ms | 55 ms | 3.2× faster |",
                     "- CPU per screen · loaded level: Ferrofin: search differs",
                     "median of 3 runs"):
            self.assertIn(text, readme)
        self.assertNotIn("\nold\n", readme)   # the block it replaced

    def test_every_graph_is_well_formed_svg_with_no_external_reference(self):
        files, _ = publish.render(str(self.published([self.summary()])), README, str(self.root))
        for path, svg in files.items():
            with self.subTest(Path(path).name):
                root = ET.fromstring(svg)
                self.assertEqual(root.get("width"), str(publish.WIDTH))
                self.assertNotRegex(svg, r"(href|src)=['\"]")
                self.assertIn("One point per Ferrofin release" if os.path.basename(path).startswith("history")
                              else "against Jellyfin 12.2.0, the median of 3 runs", svg)
        endpoints = files[next(p for p in files if p.endswith("endpoints-loaded.svg"))]
        self.assertLess(endpoints.index("home:image"), endpoints.index("playback:intros"))   # wins first
        self.assertIn(publish.STYLE["light"]["slower"], endpoints)   # the slower cell in amber
        self.assertIn("1.1× slower", endpoints)
        dark = files[next(p for p in files if p.endswith("screens-loaded-dark.svg"))]
        self.assertIn(publish.STYLE["dark"]["ferrofin"], dark)

    def test_slower_cells_are_amber_and_the_headline_says_how_many_screens_were_faster(self):
        doc = self.summary()
        bench = doc["bench"]
        bench["memory"][1]["ferrofin"]["median"] = 400   # idle memory now heavier than Jellyfin's
        bench["memory"][1]["ratio"] = round(295 / 400, 4)
        bench["ttfs"][1]["ratio"] = 0.5   # and the HLS first segment slower: the log axis reaches below 1×
        bench["levels"]["loaded"]["screens"][1]["ratio"] = 0.8
        files, readme = publish.render(str(self.published([doc])), README, str(self.root))
        memory = files[next(p for p in files if p.endswith("/memory.svg"))]
        self.assertIn(f"fill='{publish.STYLE['light']['slower']}'", memory)
        self.assertIn("1.4× heavier", memory)
        startup = files[next(p for p in files if p.endswith("/startup.svg"))]
        self.assertIn(">0.5×<", startup)
        self.assertIn("2.0× slower", startup)
        self.assertIn("**faster on 2 of 3 screens at 5 screens/s**", readme)
        self.assertIn("**2.1–3.2× the speed on 3 screens at 5 screens/s**",
                      publish.render(str(self.published([self.summary(scales=(1, 1, 1))])), README, str(self.root))[1])

    def test_names_and_reasons_cannot_break_the_tables_or_the_markup(self):
        doc = self.summary()
        doc["bench"]["levels"]["loaded"]["screens"][0]["name"] = "a|b <i>"
        doc["bench"]["levels"]["loaded"]["screens"][2].update(ratio=None, withheld="x | <script>")
        files, readme = publish.render(str(self.published([doc])), README, str(self.root))
        self.assertIn("| a\\|b &lt;i&gt; | 17 ms |", readme)
        self.assertIn("search: x \\| &lt;script&gt;", readme)
        self.assertNotIn("<script>", readme)
        for svg in files.values():
            ET.fromstring(svg)

    def test_a_summary_filed_under_another_version_is_refused(self):
        doc = self.summary()
        results = self.root / "docs" / "results"
        publish.write_json(str(results / "v1.5.0" / "summary.json"), doc)
        with self.assertRaisesRegex(publish.Refused, "holds v1.4.0's results, not v1.5.0's"):
            publish.render(str(results), README, str(self.root))

    def history(self, versions, hosts=None, fixtures=None, change=None):
        """The rendered files and README of summaries of `versions` (and only those), each from the same three
        runs, on host class `hosts[i]` and fixture `fixtures[i]` when given; `change(i, doc)` edits one."""
        base, out = self.summary(), []
        for i, version in enumerate(versions):
            doc = {**copy.deepcopy(base), "version": version}
            if hosts:
                doc["bench"]["source"]["host_class"] = hosts[i]
            if fixtures:
                doc["bench"]["source"]["fixture"] = fixtures[i]
            if change:
                change(i, doc)
            out.append(doc)
        results = self.root / "history" / str(len(list(self.root.glob("history/*"))))
        for doc in out:
            publish.write_json(str(results / doc["version"] / "summary.json"), doc)
        files, readme = publish.render(str(results), README, str(self.root))
        return files[str(results / "history.svg")], readme

    def test_one_release_is_drawn_as_points_and_named_with_its_jellyfin(self):
        svg, readme = self.history(["v1.4.0"])
        ET.fromstring(svg)
        self.assertNotIn("stroke-width='2'", svg)   # one point per server per panel: no line to draw
        for text in (">v1.4.0<", ">vs Jellyfin 12.2.0<", ">379 ms<", ">3420 ms<", ">17 ms<", "Cold start", "Idle memory"):
            self.assertIn(text, svg)
        self.assertIn('### Release over release', readme)
        self.assertIn('srcset="history/0/history-dark.svg"', readme)

    def test_two_releases_on_one_host_are_joined_and_another_host_starts_a_new_line(self):
        joined, _ = self.history(["v1.4.0", "v1.5.0"])
        self.assertEqual(joined.count("stroke-width='2'"), 2 * len(publish.HISTORY))   # Ferrofin and Jellyfin, per panel
        self.assertNotIn("new host or fixture", joined)
        same, other = {"cores": 32, "memory_gib": 125}, {"cores": 16, "memory_gib": 64}
        for kwargs in ({"hosts": [same, same, other]}, {"fixtures": ["4b21", "4b21", "77aa"]}):
            with self.subTest(**kwargs):
                split, _ = self.history(["v1.4.0", "v1.5.0", "v1.6.0"], **kwargs)
                self.assertEqual(split.count("stroke-width='2'"), 2 * len(publish.HISTORY))   # 1.4→1.5, not on to 1.6
                self.assertEqual(split.count("new host or fixture"), 1)
                self.assertEqual(split.count("stroke-dasharray"), len(publish.HISTORY))   # inside each panel's plot

    def test_releases_are_ordered_by_version_number_and_a_withheld_number_breaks_its_line(self):
        svg, _ = self.history(["v1.10.0", "v1.9.0"])
        self.assertLess(svg.index(">v1.9.0<"), svg.index(">v1.10.0<"))
        def withhold_cold_start(i, doc):
            if i == 1:
                doc["bench"]["ttfs"][0].update(ratio=None, withheld="Ferrofin: different work")
        svg, _ = self.history(["v1.4.0", "v1.5.0", "v1.6.0"], change=withhold_cold_start)
        self.assertEqual(svg.count("stroke-width='2'"), 2 * 2 * (len(publish.HISTORY) - 1))   # cold start's line is broken
        self.assertIn(">withheld<", svg)

    def test_the_newest_release_is_drawn(self):
        new = self.summary()
        old = {**copy.deepcopy(new), "version": "v1.3.10"}
        _, readme = publish.render(str(self.published([old, new])), README, str(self.root))
        self.assertIn("Ferrofin **v1.4.0**", readme)

    def test_render_refuses_without_summaries_or_markers(self):
        empty = self.root / "empty"
        empty.mkdir()
        with self.assertRaisesRegex(publish.Refused, "run summarise first"):
            publish.render(str(empty), README)
        with self.assertRaisesRegex(publish.Refused, "no <!-- BEGIN GENERATED RESULTS"):
            publish.render(str(self.published([self.summary()])), "# no block\n")

    def test_readme_check_fails_on_a_stale_block_and_passes_once_written(self):
        results = self.published([self.summary()])
        readme = self.root / "README.md"
        readme.write_text(README)
        args = ["readme", "--results", str(results), "--readme", str(readme)]
        with self.assertRaisesRegex(SystemExit, "out of date, run make publish-results"):
            publish.main([*args, "--check"])
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(publish.main(args), 0)
            self.assertEqual(publish.main([*args, "--check"]), 0)
        readme.write_text(readme.read_text().replace("3.2× faster", "4× faster"))   # a hand edit
        with self.assertRaisesRegex(SystemExit, "README.md"):
            publish.main([*args, "--check"])
        readme.write_text(README)
        with contextlib.redirect_stdout(io.StringIO()):
            publish.main(args)
        leftover = results / "v1.4.0" / "gone.svg"   # a graph the summary no longer gives
        leftover.write_text("<svg/>")
        with self.assertRaisesRegex(SystemExit, "gone.svg"):
            publish.main([*args, "--check"])
        with contextlib.redirect_stdout(io.StringIO()):
            publish.main(args)
        self.assertFalse(leftover.exists())
        with self.assertRaisesRegex(SystemExit, "publish: no .*missing: run summarise first"):
            publish.main(["readme", "--results", str(self.root / "missing"), "--readme", str(readme)])

    def test_the_adoption_status_comes_from_the_named_session(self):
        s = adoption_session(self.root, ADOPTION)
        doc = publish.summarise("v1.4.0", self.runs([bench_result(x) for x in (1.0, 1.1, 0.9)]), str(s))
        self.assertEqual(publish.summary_problems(doc), [])
        adoption = doc["adoption"]
        started = json.loads((s / "adoption" / "result.json").read_text())["started"]   # the run's own start, not today's
        self.assertEqual(adoption["source"], {"session": s.name, "commit": "5112240e1923cdc9", "image_digest": "sha256:img",
                                              "oracle": "12.2", "date": started[:10]})
        self.assertEqual([(r["fixture"], r["verdict"]) for r in adoption["rows"]],
                         [(f, v) for f, _, _, v, _ in ADOPTION])
        self.assertEqual(len(adoption["rows"][2]["reason"]), publish.REASON_CHARS)   # the full reason is in the session's report
        self.assertEqual(adoption["rows"][0]["reason"], "")
        files, readme = publish.render(str(self.published([doc])), README, str(self.root))
        grid = files[next(p for p in files if p.endswith("/adoption.svg"))]
        ET.fromstring(grid)
        for text in ("10.11.11-messy", ">failed<", ">skipped<", ">passed<", f"fill='{publish.STYLE['light']['SKIP']}'",
                     "gates session " + s.name):
            self.assertIn(text, grid)
        for text in ("### Adopting an existing Jellyfin database", "**2 of 4 passed** (1 skipped: their fixtures were not built)",
                     "(commit `5112240e1923`)",
                     'srcset="docs/results/v1.4.0/adoption-dark.svg"', "| jellyfin-12.1-from-12 | path | 12.1.0 | skipped | fixture not built |"):
            self.assertIn(text, readme)
        self.assertLess(readme.index("### Speed and memory against Jellyfin"), readme.index("### Adopting an existing"))

    def test_the_parity_overview_comes_from_the_named_session(self):
        s = adoption_session(self.root, ADOPTION)
        doc = publish.summarise("v1.4.0", self.runs([bench_result(x) for x in (1.0, 1.1, 0.9)]), str(s))
        parity = doc["parity"]
        self.assertEqual(publish.summary_problems(doc), [])
        self.assertEqual((parity["sent"], parity["counts"], parity["filed_issues"], parity["source"]["oracle"]),
                         (12, {"identical": 6, "accepted": 3, "filed": 1, "new": 1, "oracle_failed": 1}, [832], "Jellyfin 12.2.0"))
        self.assertEqual([a["area"] for a in parity["areas"]], ["/Items", "/Intros", "/Sessions", "/Videos"])
        files, readme = publish.render(str(self.published([doc])), README, str(self.root))
        graph = files[next(p for p in files if p.endswith("/parity.svg"))]
        ET.fromstring(graph)
        for text in ("/Items", ">6 of 9 identical<", ">0 of 1 identical  ·  1 Jellyfin failed<", ">0 of 1 identical  ·  1 not in ledger<",
                     ">known (in ledger)<", f"fill='{publish.STYLE['light']['new']}'",
                     "against Jellyfin 12.2.0, gates session"):
            self.assertIn(text, graph)
        for text in ("### Answering like Jellyfin", "replays the **12** requests", "**6 are identical**",
                     "**3 differ in known ways**", "**1 is filed as an issue** and **1 is new**",
                     "1 was not compared because Jellyfin itself failed it", "| /Items | 9 | 6 | 3 | 0 | 0 | 0 |",
                     "Filed as issues: gl-832.", 'srcset="docs/results/v1.4.0/parity-dark.svg"'):
            self.assertIn(text, readme)
        self.assertLess(readme.index("### Adopting an existing"), readme.index("### Answering like Jellyfin"))
        self.assertIn("**The parity gate did not pass** (new differences): 2 requests differ in ways the ledger does "
                      "not cover yet.", readme)

    def test_a_passing_parity_run_says_so(self):
        doc = publish.summarise("v1.4.0", self.runs([bench_result(x) for x in (1.0, 1.1, 0.9)]),
                                str(adoption_session(self.root, ADOPTION)))
        doc["parity"]["verdict"] = "pass"
        _, readme = publish.render(str(self.published([doc])), README, str(self.root))
        self.assertIn("**The parity gate passed**: the ledger covers every difference.", readme)

    def test_results_of_another_commit_than_the_tag_are_refused_unless_published_as_measured_before_it(self):
        runs = self.runs([bench_result(x) for x in (1.0, 1.1, 0.9)])
        s = str(adoption_session(self.root, ADOPTION))
        same = publish.summarise("v1.4.0", runs, s, tag="5112240e1923cdc9aaaa")
        self.assertEqual((same["tag"], publish.summary_problems(same)), ({"commit": "5112240e1923cdc9aaaa"}, []))
        _, readme = publish.render(str(self.published([same])), README, str(self.root))
        self.assertNotIn("Measured before", readme)
        with self.assertRaisesRegex(publish.Refused, "the benchmark runs measured 5112240e, but the v1.4.0 tag is "
                                                     "d2825f0bd978: .*--measured-before-tag"):
            publish.summarise("v1.4.0", runs, s, tag="d2825f0bd97808cd")
        before = publish.summarise("v1.4.0", runs, s, tag="d2825f0bd97808cd", before_tag=True)
        _, readme = publish.render(str(self.published([before])), README, str(self.root))
        self.assertIn("> Measured before the v1.4.0 tag: results below come from commit `5112240e`, "
                      "not the tag's (`d2825f0bd978`).", readme)
        before["tag"] = {"commit": ""}
        self.assertEqual(publish.summary_problems(before), ["tag is not an object with the tag's commit"])

    def test_an_ab_gate_of_another_commit_is_refused_and_two_earlier_commits_are_both_named(self):
        runs = self.runs([bench_result(x) for x in (1.0, 1.1, 0.9)])
        s = str(adoption_session(self.root, ADOPTION))
        ab = str(ab_session(self.root / "ab", AB_VERDICTS))
        doc = publish.summarise("v1.4.0", runs, s, ab)
        doc["ab"]["source"]["commit"] = "8f052092ca2f"
        with mock.patch.object(publish, "ab_summary", return_value=doc["ab"]):
            with self.assertRaisesRegex(publish.Refused, "the A/B gate measured 8f052092ca2f, but the v1.4.0 tag is 5112240e1923"):
                publish.summarise("v1.4.0", runs, s, ab, tag="5112240e1923cdc9aaaa")
            before = publish.summarise("v1.4.0", runs, s, ab, tag="d2825f0bd97808cd", before_tag=True)
        unrecorded = copy.deepcopy(doc["ab"])
        unrecorded["source"]["commit"] = None
        with mock.patch.object(publish, "ab_summary", return_value=unrecorded), \
                self.assertRaisesRegex(publish.Refused, "the A/B gate measured an unrecorded commit"):
            publish.summarise("v1.4.0", runs, s, ab, tag="5112240e1923cdc9aaaa")
        _, readme = publish.render(str(self.published([before])), README, str(self.root))
        self.assertIn("results below come from commits `5112240e` and `8f052092ca2f`, not the tag's (`d2825f0bd978`).", readme)

    def test_a_parity_run_with_one_untracked_request_reads_in_the_singular(self):
        parity = {"verdict": "new differences", "counts": {"filed": 0, "new": 1}}
        self.assertEqual(publish.parity_verdict(parity), "**The parity gate did not pass** (new differences): 1 request "
                                                         "differs in ways the ledger does not cover yet.")

    def test_tag_commit_names_the_commit_of_an_annotated_or_lightweight_tag(self):
        repo = self.root / "repo"
        # the user's own git config (signing, hooks) must not reach the throwaway repository
        env = {**{k: v for k, v in os.environ.items() if not k.startswith("GIT_")},
               "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}
        git = lambda *a: subprocess.run(["git", "-C", str(repo), *a], check=True, capture_output=True, text=True,
                                        env=env).stdout.strip()
        repo.mkdir()
        git("init", "-q")
        git("-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-q", "--allow-empty", "-m", "x")
        head = git("rev-parse", "HEAD")
        git("-c", "user.name=t", "-c", "user.email=t@example.com", "tag", "-a", "v9.9.9", "-m", "annotated")
        git("tag", "v9.9.8")
        self.assertEqual((publish.tag_commit("v9.9.9", str(repo)), publish.tag_commit("v9.9.8", str(repo))), (head, head))
        self.assertIsNone(publish.tag_commit("v9.9.7", str(repo)))
        self.assertIsNone(publish.tag_commit("v9.9.9", str(self.root)))   # not a repository at all

    def test_the_command_line_publishes_results_measured_before_the_tag_only_when_told_to(self):
        out, paths = self.root / "out", self.runs([bench_result(s) for s in (1.0, 1.1, 0.9)])
        args = ["summarise", "--version", "v1.4.0", "--bench", *paths, "--out", str(out)]
        with mock.patch.object(publish, "tag_commit", return_value="d2825f0bd97808cd"):
            with self.assertRaisesRegex(SystemExit, "--measured-before-tag"):
                publish.main(args)
            with contextlib.redirect_stdout(io.StringIO()):
                publish.main([*args, "--measured-before-tag"])
        self.assertEqual(json.loads((out / "v1.4.0" / "summary.json").read_text())["tag"], {"commit": "d2825f0bd97808cd"})
        with self.assertRaisesRegex(SystemExit, "'1.4' is not a release version"):
            publish.main(["summarise", "--version", "1.4", "--bench", "x"])

    def test_summarise_refuses_a_release_this_checkout_has_not_tagged(self):
        with mock.patch.object(publish, "tag_commit", return_value=None), \
                self.assertRaisesRegex(SystemExit, "no v1.4.0 tag: results are published after the tag"):
            publish.main(["summarise", "--version", "v1.4.0", "--bench", "x", "--out", str(self.root)])

    def test_the_parity_graph_sums_the_quiet_areas_into_one_bar(self):
        areas = [{"area": f"/A{i:02d}", "requests": 30 - i, "identical": 30 - i, "accepted": 0, "filed": 0, "new": 0,
                  "oracle_failed": 0} for i in range(publish.PARITY_AREAS + 3)]
        rows = publish.parity_rows(areas)
        self.assertEqual(len(rows), publish.PARITY_AREAS + 1)
        self.assertEqual((rows[-1]["area"], rows[-1]["requests"]), ("3 other areas", 18 + 17 + 16))

    def test_an_area_says_how_many_of_its_requests_are_identical_and_names_the_thin_segments(self):
        parity = {"areas": [{"area": "/Items", "requests": 490, "identical": 151, "accepted": 333, "filed": 0, "new": 1,
                             "oracle_failed": 5},
                            {"area": "/Shows", "requests": 325, "identical": 76, "accepted": 249, "filed": 0, "new": 0,
                             "oracle_failed": 0}]}
        graph = publish.parity_graph("t", parity, "c", "light")
        self.assertIn(">151 of 490 identical  ·  1 not in ledger  ·  5 Jellyfin failed<", graph)
        self.assertIn(">76 of 325 identical<", graph)   # a known difference is not counted as alike
        self.assertNotIn("alike", graph)
        widest = max(float(x) + len(t) * int(size) * 0.58
                     for x, size, t in re.findall(r"<text x='([\d.]+)'[^>]*font-size='(\d+)'[^>]*>([^<]*)</text>", graph))
        self.assertLessEqual(widest, publish.WIDTH)   # the labels and the legend fit (by the same width estimate)

    def test_a_session_without_a_parity_result_or_one_that_compared_nothing_is_refused(self):
        runs = self.runs([bench_result(x) for x in (1.0, 1.1, 0.9)])
        bare = adoption_session(self.root / "bare", ADOPTION, parity=False)
        with self.assertRaisesRegex(publish.Refused, "parity has no result.json .*diff.py result --out"):
            publish.summarise("v1.4.0", runs, str(bare))
        failed = adoption_session(self.root / "failed", ADOPTION, parity=False)
        (failed / "parity").mkdir()
        parity_diff.gate_result().write(str(failed / "parity"), parity_diff.result_doc(
            {"verdict": "run failed", "reason": "ferrofin did not start", "planned": 0, "requests": []}))
        with self.assertRaisesRegex(publish.Refused, "did not finish .ferrofin did not start; 0 of 0 requests sent."):
            publish.summarise("v1.4.0", runs, str(failed))
        for change, refusal in ((lambda d: d["detail"].update(sent=10), "did not finish .* 10 of 12 requests sent"),
                                (lambda d: d["source"].update(commit="deadbeef"), "measured deadbeef, the benchmark runs 5112240e"),
                                (lambda d: d["detail"].update(oracle_version=None), "records no Jellyfin version")):
            s = adoption_session(self.root / refusal[:6].replace(" ", ""), ADOPTION)
            doc = json.loads((s / "parity" / "result.json").read_text())
            change(doc)
            (s / "parity" / "result.json").write_text(json.dumps(doc))
            with self.subTest(refusal), self.assertRaisesRegex(publish.Refused, refusal):
                publish.summarise("v1.4.0", runs, str(s))

    def test_an_adoption_session_of_other_or_uncommitted_code_is_refused(self):
        runs = self.runs([bench_result(x) for x in (1.0, 1.1, 0.9)])
        for kwargs, refusal in (({"commit": "deadbeef0000"}, "measured deadbeef0000, the benchmark runs 5112240e"),
                                ({"dirty": True}, "ran with uncommitted changes")):
            with self.subTest(refusal), self.assertRaisesRegex(publish.Refused, refusal):
                publish.summarise("v1.4.0", runs, str(adoption_session(self.root / refusal[:4], ADOPTION, **kwargs)))
        skipped = adoption_session(self.root / "skipped", [("jellyfin-12.2-fresh", "path", "12.2.0", "SKIP", "fixture not built")])
        with self.assertRaisesRegex(publish.Refused, "ran nothing"):
            publish.summarise("v1.4.0", runs, str(skipped))
        with self.assertRaisesRegex(publish.Refused, "is not a gates session"):
            publish.summarise("v1.4.0", runs, str(self.root))
        bare = adoption_session(self.root / "bare", ADOPTION)
        (bare / "adoption" / "result.json").unlink()
        with self.assertRaisesRegex(publish.Refused, "has no result.json .*adoption_report.py build"):
            publish.summarise("v1.4.0", runs, str(bare))

    def test_the_previous_release_comes_from_the_release_ab_gate(self):
        s = ab_session(self.root, AB_VERDICTS, blocking=["memory/peak under load"])
        doc = publish.summarise("v1.4.0", self.runs([bench_result(x) for x in (1.0, 1.1, 0.9)]), ab=str(s))
        self.assertEqual(publish.summary_problems(doc), [])
        ab = doc["ab"]
        self.assertEqual((ab["source"]["previous"], ab["source"]["candidate"], ab["source"]["pairs"], ab["verdict"]),
                         ("ghcr.io/mangoleaf/ferrofin:v1.3.2", "ferrofin:v1.4.0-rc", 3, "blocked"))
        self.assertEqual([c["cell"] for c in ab["cells"]], sorted(k for k, v in AB_VERDICTS.items() if v["gated"]))
        files, readme = publish.render(str(self.published([doc])), README, str(self.root))
        graph = files[next(p for p in files if p.endswith("/previous-release.svg"))]
        ET.fromstring(graph)
        # a bar of every gated cell by verdict, then each cell that improved or regressed, the biggest gain first
        self.assertIn(">5 gated cells: 2 improved  ·  1 within noise  ·  1 regressed  ·  1 not comparable<", graph)
        order = [graph.index(f">{cell}<") for cell in ("loaded/screens/home", "loaded/endpoints/items", "memory/peak under load")]
        self.assertEqual(order, sorted(order))
        self.assertNotIn(">loaded/screens/movies<", graph)   # within noise: counted and in the table, not drawn
        self.assertIn(">loaded/screens/search<", graph)   # not comparable blocks like a regression: named
        self.assertIn(">not comparable: blocks like a regression<", graph)
        quiet = publish.ab_graph("t", [AB_VERDICTS["loaded/screens/movies"] | {"cell": "loaded/screens/movies"}], "v1.3.2", "c", "light")
        ET.fromstring(quiet)
        self.assertIn(">1 gated cell: 1 within noise<", quiet)
        self.assertNotIn("times better", quiet)   # no bar, so no axis
        self.assertIn("2.0× faster  ·  10.0 ms vs 20.0 ms  ·  improved", graph)
        self.assertIn("1.2× heavier  ·  125 MiB vs 100 MiB  ·  regressed", graph)
        self.assertIn(f"fill='{publish.STYLE['light']['FAIL']}'", graph)   # the regression in its own colour
        self.assertNotIn("loaded/endpoints/x", graph)   # not gated
        self.assertIn(">Ferrofin v1.4.0 against Ferrofin v1.3.2<", graph)
        widest = max(float(x) + len(t) * 11 * 0.58 for x, t in re.findall(r"<text x='([\d.]+)'[^>]*font-size='11'[^>]*>([^<]*)</text>", graph))
        self.assertLessEqual(widest, publish.WIDTH)   # every bar's note fits the graph (by the same width estimate)
        for text in ("### Against the previous release", "**Ferrofin v1.3.2** (`ghcr.io/mangoleaf/ferrofin:v1.3.2`)", "3 interleaved pairs",
                     "of 5 gated cells, **2 improved**, 1 is within noise and **1 regressed**, and 1 could not be compared "
                     "(it blocks like a regression); **1 gated cell blocked the release**. The graph draws the cells "
                     "that improved or regressed; every cell is in the table under it.",
                     "| loaded/screens/movies | 10.0 ms | 10.2 ms | +2.0% | within noise |",
                     "| memory/peak under load | 100 MiB | 125 MiB | +25.0% | regressed |",
                     "| loaded/screens/search | — | — | — | not comparable: other work |",
                     'srcset="docs/results/v1.4.0/previous-release-dark.svg"'):
            self.assertIn(text, readme)
        self.assertLess(readme.index("### Speed and memory against Jellyfin"), readme.index("### Against the previous release"))

    def test_an_older_session_counts_its_pairs_from_its_cells_and_fewer_than_three_are_refused(self):
        s = ab_session(self.root / "old", AB_VERDICTS)
        doc = json.loads((s / "bench" / "result.json").read_text())
        del doc["detail"]["runs"]
        for v in doc["detail"]["verdicts"].values():
            v["b"] = [1, 2, 3]
        (s / "bench" / "result.json").write_text(json.dumps(doc))
        self.assertEqual(publish.ab_summary(str(s))["source"]["pairs"], 3)
        for v in doc["detail"]["verdicts"].values():
            v["b"] = [1, 2]
        (s / "bench" / "result.json").write_text(json.dumps(doc))
        with self.assertRaisesRegex(publish.Refused, "ran 2 pairs: a release A/B gate is at least 3"):
            publish.ab_summary(str(s))

    def test_a_regressed_cell_with_a_missing_median_is_listed_not_drawn(self):
        verdicts = {**AB_VERDICTS, "memory/steady idle": {"kind": "memory", "unit": "MiB", "a_median": 0, "b_median": 90,
                                                          "verdict": "regressed", "gated": True}}
        doc = publish.summarise("v1.4.0", self.runs([bench_result(x) for x in (1.0, 1.1, 0.9)]),
                                ab=str(ab_session(self.root / "zero", verdicts)))
        files, readme = publish.render(str(self.published([doc])), README, str(self.root))
        self.assertNotIn("memory/steady idle", files[next(p for p in files if p.endswith("/previous-release.svg"))])
        self.assertIn("| memory/steady idle | 0.00 MiB | 90.0 MiB | — | regressed (not drawn: a median is missing) |", readme)

    def test_summarise_refuses_a_session_that_is_not_an_ab_gate_or_judged_nothing(self):
        runs = self.runs([bench_result(x) for x in (1.0, 1.1, 0.9)])
        with self.assertRaisesRegex(publish.Refused, "is not an A/B gate session .its mode is 'aa'."):
            publish.summarise("v1.4.0", runs, ab=str(ab_session(self.root / "aa", AB_VERDICTS, mode="aa")))
        with self.assertRaisesRegex(publish.Refused, "judged no gated cell"):
            publish.summarise("v1.4.0", runs, ab=str(ab_session(self.root / "none", {"x": {"gated": False, "verdict": "improved"}})))
        with self.assertRaisesRegex(publish.Refused, "has no result.json .*gate.py render"):
            publish.summarise("v1.4.0", runs, ab=str(self.root))

    def test_summarise_writes_the_release_file(self):
        out = self.root / "out"
        paths = self.runs([bench_result(s) for s in (1.0, 1.1, 0.9)])
        tag = "5112240e1923cdc9aaaa"   # the commit the runs measured, whatever this checkout's own v1.4.0 tag names
        with mock.patch.object(publish, "tag_commit", return_value=tag) as lookup, \
                contextlib.redirect_stdout(io.StringIO()) as printed:
            publish.main(["summarise", "--version", "v1.4.0", "--bench", *paths, "--out", str(out)])
        lookup.assert_called_once_with("v1.4.0")
        written = out / "v1.4.0" / "summary.json"
        self.assertEqual(printed.getvalue().strip(), str(written))
        self.assertEqual(json.loads(written.read_text()), publish.summarise("v1.4.0", paths, tag=tag))

    def test_session_graphs_inline_both_themes_of_one_run(self):
        html = publish.session_graphs(bench_result())
        self.assertEqual(html.count("<div class='graph light'><svg"), html.count("<div class='graph dark'><svg"))
        self.assertIn("the median of 1 run", html)
        self.assertIn("Ferrofin (commit 5112240e) against", html)   # a session's build has no release name
        self.assertNotIn("<img", html)


if __name__ == "__main__":
    unittest.main()

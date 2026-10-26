"""gates/publish.py: the release summary from fixed benchmark results, and the README Results block and its
graphs from fixed summary files (stdlib)."""

import contextlib
import copy
import io
import json
import os
import sys
import tempfile
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path

GATES = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(GATES))
import publish  # noqa: E402
import result  # noqa: E402
import session  # noqa: E402

sys.path.insert(0, str(GATES / "adoption"))
import adoption_report  # noqa: E402
sys.path.insert(0, str(GATES / "parity"))
import diff as parity_diff  # noqa: E402

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
                self.assertIn("against Jellyfin 12.2.0, the median of 3 runs", svg)
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
        self.assertEqual(adoption["source"], {"session": s.name, "commit": "5112240e1923cdc9", "image_digest": "sha256:img",
                                              "oracle": "12.2", "date": "2026-10-26"})
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
        for text in ("/Items", "9  ·  100% alike", "/Intros", "not compared", f"fill='{publish.STYLE['light']['new']}'",
                     "against Jellyfin 12.2.0, gates session"):
            self.assertIn(text, graph)
        for text in ("### Answering like Jellyfin", "replays the **12** requests", "**6 are identical**",
                     "**3 differ in accepted ways**", "**1 is filed as an issue** and **1 is new**",
                     "1 was not compared because Jellyfin itself failed it", "| /Items | 9 | 6 | 3 | 0 | 0 | 0 |",
                     "Filed as issues: gl-832.", 'srcset="docs/results/v1.4.0/parity-dark.svg"'):
            self.assertIn(text, readme)
        self.assertLess(readme.index("### Adopting an existing"), readme.index("### Answering like Jellyfin"))

    def test_the_parity_graph_sums_the_quiet_areas_into_one_bar(self):
        areas = [{"area": f"/A{i:02d}", "requests": 30 - i, "identical": 30 - i, "accepted": 0, "filed": 0, "new": 0,
                  "oracle_failed": 0} for i in range(publish.PARITY_AREAS + 3)]
        rows = publish.parity_rows(areas)
        self.assertEqual(len(rows), publish.PARITY_AREAS + 1)
        self.assertEqual((rows[-1]["area"], rows[-1]["requests"]), ("3 other areas", 18 + 17 + 16))

    def test_an_area_reads_100_percent_alike_only_when_every_compared_request_is(self):
        parity = {"areas": [{"area": "/Items", "requests": 490, "identical": 151, "accepted": 333, "filed": 0, "new": 1,
                             "oracle_failed": 5}]}
        self.assertIn("490  ·  99.7% alike", publish.parity_graph("t", parity, "c", "light"))

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

    def test_summarise_writes_the_release_file(self):
        out = self.root / "out"
        paths = self.runs([bench_result(s) for s in (1.0, 1.1, 0.9)])
        with contextlib.redirect_stdout(io.StringIO()) as printed:
            publish.main(["summarise", "--version", "v1.4.0", "--bench", *paths, "--out", str(out)])
        written = out / "v1.4.0" / "summary.json"
        self.assertEqual(printed.getvalue().strip(), str(written))
        self.assertEqual(json.loads(written.read_text()), publish.summarise("v1.4.0", paths))

    def test_session_graphs_inline_both_themes_of_one_run(self):
        html = publish.session_graphs(bench_result())
        self.assertEqual(html.count("<div class='graph light'><svg"), html.count("<div class='graph dark'><svg"))
        self.assertIn("the median of 1 run", html)
        self.assertNotIn("<img", html)


if __name__ == "__main__":
    unittest.main()

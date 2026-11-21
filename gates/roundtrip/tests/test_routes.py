"""routes.sh against the fixtures gates/adoption builds: there is a route for every fixture, and each names a
fixture build-fixtures.sh builds, the generation Ferrofin adopts it as, the log of Jellyfin's own first start
on its source that build-fixtures.sh writes, the directories Ferrofin runs in and its kind."""

from pathlib import Path
import subprocess
import sys
import unittest

ROUNDTRIP = Path(__file__).resolve().parent.parent
sys.path.append(str(ROUNDTRIP.parent / "adoption"))
import messy  # noqa: E402
from preservation import GENERATIONS  # noqa: E402
import scale  # noqa: E402
from synthetic import FIRST_STARTS, LAYOUTS, RETURN_RELEASE, VERSIONS  # noqa: E402

#: The copies messy.py and scale.py derive, {fixture: (source, kind)}.
DERIVED = {**{name: (source, "messy") for name, source, _ in messy.DERIVED}, scale.DERIVED[0]: (scale.DERIVED[1], "scale")}


def routes():
    """[(route, fixture, generation, first start, data directory, configuration directory, kind)], as run.sh
    reads them."""
    printed = subprocess.run(["bash", "-c", '. "$1"; printf "%s\\n" "${ROUNDTRIP_ROUTES[@]}"', "_",
                              str(ROUNDTRIP / "routes.sh")], check=True, capture_output=True, text=True).stdout
    return [tuple(line.split("|")) for line in printed.splitlines()]


def moved_dirs():
    """(data, configuration): the directories gates/adoption/run.sh adopts a fixture of another layout into."""
    printed = subprocess.run(["bash", "-c", '. "$1"; printf "%s\\n" "$ADOPTION_MOVED_DATA" "$ADOPTION_MOVED_CONFIG"', "_",
                              str(ROUNDTRIP.parent / "adoption" / "lib.sh")], check=True, capture_output=True, text=True).stdout
    return tuple(printed.split())


class Routes(unittest.TestCase):
    def test_each_route_returns_to_jellyfin_s_own_first_start_on_its_source(self):
        built = {name: (version, parent) for name, version, parent in VERSIONS}
        for route, fixture, generation, first_start, ran_at, config_at, kind in routes():
            with self.subTest(route=route):
                self.assertEqual(route, fixture.removeprefix("jellyfin-"))
                source, expected_kind = DERIVED.get(fixture, (fixture, "path"))
                self.assertEqual(kind, expected_kind)
                version, _ = built[source]
                self.assertEqual(generation, GENERATIONS[version])
                if source in FIRST_STARTS:
                    self.assertEqual(first_start, f"oracle/first-start-{RETURN_RELEASE}/{source}")
                else:
                    # a fixture of that release whose build was its first start on the source, or the
                    # source itself when it is one: what it ran there, the source's history records
                    release, parent = built[first_start]
                    self.assertEqual(release, RETURN_RELEASE)
                    self.assertIn(source, (parent, first_start))
                self.assertTrue(ran_at.startswith("/"))
                # a fixture Jellyfin wrote in another layout runs with its configuration apart, as a
                # package migration leaves it; every other one in its data directory
                if fixture in LAYOUTS:   # where the adoption gate adopts it (gates/adoption/lib.sh ADOPTION_MOVED_*)
                    self.assertEqual((ran_at, config_at), moved_dirs())
                else:
                    self.assertEqual(config_at, "-")

    def test_every_fixture_build_fixtures_sh_builds_is_a_route(self):
        fixtures = {route[1] for route in routes()}
        self.assertEqual(fixtures, {name for name, _, _ in VERSIONS} | set(DERIVED))

    def test_routes_are_named_once(self):
        names = [route[0] for route in routes()]
        self.assertEqual(len(names), len(set(names)))

    def test_every_older_fixture_no_fixture_of_the_release_upgrades_gets_a_first_start(self):
        self.assertEqual(FIRST_STARTS, (
            "jellyfin-10.11.9", "jellyfin-10.11.10", "jellyfin-10.11.11", "jellyfin-12.0",
            "jellyfin-12.1-from-10", "jellyfin-12.1-from-12", "jellyfin-12.2-from-10", "jellyfin-12.2-from-12",
            "jellyfin-12.2-from-121", "jellyfin-12.2-fresh", "jellyfin-12.3-from-10", "jellyfin-12.3-fresh"))


if __name__ == "__main__":
    unittest.main()

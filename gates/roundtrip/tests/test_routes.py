"""routes.sh against the fixtures gates/adoption builds: each route names a fixture
build-fixtures.sh builds, the generation Ferrofin adopts it as, and the log of Jellyfin's own first start on
it that build-fixtures.sh writes."""

from pathlib import Path
import subprocess
import sys
import unittest

ROUNDTRIP = Path(__file__).resolve().parent.parent
sys.path.append(str(ROUNDTRIP.parent / "adoption"))
from preservation import GENERATIONS  # noqa: E402
from synthetic import FIRST_STARTS, RETURN_RELEASE, VERSIONS  # noqa: E402


def routes():
    """[(route, fixture, generation, first start, data directory)], as run.sh reads them."""
    printed = subprocess.run(["bash", "-c", '. "$1"; printf "%s\\n" "${ROUNDTRIP_ROUTES[@]}"', "_",
                              str(ROUNDTRIP / "routes.sh")], check=True, capture_output=True, text=True).stdout
    return [tuple(line.split("|")) for line in printed.splitlines()]


class Routes(unittest.TestCase):
    def test_each_route_returns_to_jellyfin_s_own_first_start_on_its_source(self):
        built = {name: (version, parent) for name, version, parent in VERSIONS}
        for route, fixture, generation, first_start, ran_at in routes():
            with self.subTest(route=route):
                self.assertEqual(route, fixture.removeprefix("jellyfin-"))
                version, _ = built[fixture]
                self.assertEqual(generation, GENERATIONS[version])
                if fixture in FIRST_STARTS:
                    self.assertEqual(first_start, f"oracle/first-start-{RETURN_RELEASE}/{fixture}")
                else:
                    # a fixture of that release whose build was its first start on the source, or the
                    # source itself when it is one: that release's own first start on its set of ids
                    release, parent = built[first_start]
                    self.assertEqual(release, RETURN_RELEASE)
                    self.assertIn(fixture, (parent, first_start))
                self.assertTrue(ran_at.startswith("/"))

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

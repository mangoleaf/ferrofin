#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
# Adapted from the ZapLink scanner by Kenneth Mitchell.
# https://github.com/kmitchel/zaplink/blob/8011d54/src/scanner.c
# Original MIT notice: see scan-linux-dvb.NOTICE.
"""Scan U.S. ATSC 1.0 channels for the Ferrofin Linux DVB tuner."""

from argparse import ArgumentParser, ArgumentTypeError
from concurrent.futures import ThreadPoolExecutor
from html import unescape
import os
from pathlib import Path
import re
import shutil
import statistics
import subprocess
import tempfile
from urllib.parse import urlencode
from urllib.request import urlopen


def rf_frequency(channel):
    if channel <= 4:
        return 57_000_000 + (channel - 2) * 6_000_000
    if channel <= 6:
        return 79_000_000 + (channel - 5) * 6_000_000
    if channel <= 13:
        return 177_000_000 + (channel - 7) * 6_000_000
    return 473_000_000 + (channel - 14) * 6_000_000


def rf_channel(value):
    try:
        channel = int(value)
    except ValueError:
        raise ArgumentTypeError("RF channel must be an integer from 2 to 36") from None
    if not 2 <= channel <= 36:
        raise ArgumentTypeError("RF channel must be from 2 to 36")
    return channel


def zip_code(value):
    if not re.fullmatch(r"[0-9]{5}", value):
        raise ArgumentTypeError("ZIP code must contain exactly five digits")
    return value


def parse_rf_hints(page):
    channels = set()
    for cell in re.findall(r"<td\b[^>]*>([^<]*)</td\s*>", page, re.IGNORECASE):
        # RabbitEars cells show the virtual channel followed by the physical RF channel.
        match = re.fullmatch(r"\s*[0-9]+[-\u2011.][0-9]+\s*\(([0-9]{1,2})\)\s*", unescape(cell))
        if match and 2 <= (channel := int(match[1])) <= 36:
            channels.add(channel)
    return channels


def lookup_rf_hints(zipcode):
    query = urlencode({"request": "zip_search", "zipcode": zipcode})
    limit = 8 * 1024 * 1024
    with urlopen(f"https://www.rabbitears.info/search.php?{query}", timeout=15) as response:
        page = response.read(limit + 1)
    if len(page) > limit:
        raise ValueError("RabbitEars response exceeded 8 MiB")
    return parse_rf_hints(page.decode("utf-8", errors="replace"))


def select_channels(rf, zipcode, skip_vhf):
    allowed = set(range(14 if skip_vhf else 2, 37))
    if rf is not None:
        return sorted(allowed.intersection(rf))
    if zipcode is not None:
        try:
            hints = allowed.intersection(lookup_rf_hints(zipcode))
        except (OSError, ValueError) as error:
            print(f"RabbitEars lookup failed ({error}); scanning the full selected band.")
        else:
            if hints:
                print(f"RabbitEars ZIP {zipcode}: scanning {len(hints)} candidate RF channels.")
                return sorted(hints)
            print("RabbitEars returned no usable RF channels; scanning the full selected band.")
    return sorted(allowed)


def scan(adapter, channels, work):
    initial = work / f"adapter{adapter}-frequencies.conf"
    result = work / f"adapter{adapter}-services.conf"
    initial.write_text("".join(
        f"[RF{channel}]\nDELIVERY_SYSTEM = ATSC\nFREQUENCY = {rf_frequency(channel)}\n"
        "MODULATION = VSB/8\n\n"
        for channel in channels
    ), encoding="utf-8")
    subprocess.run(
        ["dvbv5-scan", "-a", str(adapter), "-F", "-T", "0.5", "-C", "US",
         "-o", str(result), str(initial)],
        stdin=subprocess.DEVNULL, check=True, timeout=900,
    )
    return result.read_text(encoding="utf-8", errors="replace")


def services(text):
    """Read complete dvbv5 sections while preserving their tuning fields."""
    sections = list(re.finditer(r"(?m)^[^\S\n]*\[([^\]\n]+)\][^\S\n]*$", text))
    ends = [section.start() for section in sections[1:]] + [len(text)]
    for match, end in zip(sections, ends):
        body = text[match.end():end].strip()
        fields = dict(
            (key.strip(), value.strip())
            for line in body.splitlines()
            if not line.lstrip().startswith(("#", ";"))
            for key, sep, value in [line.partition("=")]
            if sep
        )
        try:
            number = fields["VCHANNEL"]
            frequency = int(fields["FREQUENCY"])
            raw_id = fields["SERVICE_ID"]
            service = int(raw_id, 16 if raw_id.lower().startswith("0x") else 10)
            if frequency < 1 or not 1 <= service <= 65535 or not number:
                raise ValueError
        except (KeyError, ValueError):
            print(f"Skipping {match[1]}: incomplete or invalid VCHANNEL, FREQUENCY or SERVICE_ID")
            continue
        yield number, frequency, service, match[1], body


def measure(adapter, station, work):
    _, _, service, _, body = station
    config = work / "signal.conf"
    tune_fields = [line for line in body.splitlines()
                   if line.partition("=")[0].strip() != "SERVICE_ID"]
    config.write_text(f"[ferrofin-scan]\nSERVICE_ID = {service}\n"
                      + "\n".join(tune_fields) + "\n", encoding="utf-8")
    try:
        result = subprocess.run(
            ["dvbv5-zap", "-a", str(adapter), "-c", str(config),
             "-x", "-v", "-t", "10", "ferrofin-scan"],
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            text=True, errors="replace", check=False, timeout=12,
        )
    except subprocess.TimeoutExpired:
        return "no tuner lock", True
    locked = [line for line in result.stdout.splitlines() if re.search(r"\bLock\s*\(", line)]
    if result.returncode or not locked:
        return "no tuner lock", True
    samples = [
        float(match[1])
        for line in locked
        if (match := re.search(r"C/N=\s*(-?\d+(?:\.\d+)?)\s*dB\b", line))
    ]
    if not samples:
        return "locked, driver did not report C/N in dB", False
    cnr = statistics.mean(samples)
    return f"{cnr:.1f} dB C/N", cnr < 20


def build_lineup(scan_results, adapters, keep_weak, work):
    muxes = {}
    for station in services("\n".join(scan_results)):
        muxes.setdefault(station[1], []).append(station)
    active = set()
    blocks = []
    for frequency, stations in sorted(muxes.items()):
        for adapter in adapters:
            detail, weak = measure(adapter, stations[0], work)
            if detail != "no tuner lock":
                break
        print(f"{frequency / 1e6:g} MHz on adapter {adapter}: {detail}")
        for number, _, _, name, body in stations:
            block = f"[{name}]\n{body}"
            if weak and not keep_weak:
                block = f"# Weak multiplex: {detail}\n" + "\n".join(
                    f"# {line}" for line in block.splitlines()
                )
            else:
                if number in active:
                    raise ValueError(f"duplicate virtual channel {number}; narrow the RF scan")
                active.add(number)
            blocks.append(block)
    if not active:
        raise ValueError("scan found no usable services with a virtual channel number")
    return "\n\n".join(blocks) + "\n", len(active)


def install(output, text):
    with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=output.parent) as candidate:
        candidate.write(text)
        candidate.flush()
        # Publish atomically and fail if another writer created output.
        os.link(candidate.name, output)


def main():
    parser = ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path, help="path for the generated channels.conf")
    parser.add_argument("--adapter", type=int, action="append",
                        help="adapter number, repeat for parallel scans; default: all detected")
    frequencies = parser.add_mutually_exclusive_group()
    frequencies.add_argument("--rf", type=rf_channel, nargs="+",
                             help="scan only these RF channels; default: 2 through 36")
    frequencies.add_argument("--zip", type=zip_code, metavar="ZIP",
                             help="use RabbitEars RF channel hints for a five-digit U.S. ZIP code")
    parser.add_argument("--skip-vhf", action="store_true", help="skip RF channels 2 through 13")
    parser.add_argument("--include-weak", action="store_true",
                        help="keep multiplexes below 20 dB C/N or without a measured lock")
    args = parser.parse_args()
    for program in ("dvbv5-scan", "dvbv5-zap"):
        if not shutil.which(program):
            parser.error(f"{program} is required (install v4l-utils)")

    detected = sorted(int(match[1]) for path in Path("/dev/dvb").glob("adapter*")
                      if (match := re.fullmatch(r"adapter(\d+)", path.name))
                      and all(os.access(path / device, os.R_OK | os.W_OK)
                              for device in ("frontend0", "demux0", "dvr0")))
    adapters = sorted(set(args.adapter if args.adapter is not None else detected))
    if not adapters or any(adapter not in detected for adapter in adapters):
        parser.error("no requested DVB adapters are accessible; check /dev/dvb permissions")
    output = args.output.absolute()
    if not output.parent.is_dir():
        parser.error(f"output directory does not exist: {output.parent}")
    if output.is_symlink() or (output.exists() and not output.is_file()):
        parser.error("output must be a regular file path")
    if output.exists():
        parser.error(f"{output} already exists; scan to a new path and install it after review")

    channels = select_channels(args.rf, args.zip, args.skip_vhf)
    if not channels:
        parser.error("no RF channels selected")
    adapters = adapters[:len(channels)]

    try:
        with tempfile.TemporaryDirectory(prefix="ferrofin-dvb-scan-") as directory:
            work = Path(directory)
            print(f"Scanning {len(channels)} RF channels on adapters {adapters}", flush=True)
            with ThreadPoolExecutor(max_workers=len(adapters)) as workers:
                jobs = [workers.submit(scan, adapter, channels[index::len(adapters)], work)
                        for index, adapter in enumerate(adapters)]
                results = [job.result() for job in jobs]
            text, count = build_lineup(results, adapters, args.include_weak, work)
            install(output, text)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        parser.exit(1, f"Scan failed: {error}\n")
    print(f"Wrote {count} channels to {output}")


if __name__ == "__main__":
    main()

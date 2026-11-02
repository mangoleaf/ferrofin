#!/usr/bin/env python3
"""Emit safe ICU4X search payloads from an immutable official export archive.

No executable source generator is invoked. Array/header values are copied exactly,
CES signed TOML bit patterns become u64, metadata sets the tailoring-present bit
exactly as ICU4X SourceDataProvider does. SPDX-License-Identifier: Unicode-3.0.
"""
import argparse
import hashlib
import json
import pathlib
import tomllib
import zipfile
import re
import xml.etree.ElementTree as ET

EXPECTED = "486751335ea00fb12cb3b4d52ddefd3df9b61ff81e1546ba506d3c41cec45a54"


def arr(values):
    return "&[" + ", ".join(str(v) for v in values) + "]"


def raw(data, locale="und", attribute="", bits=0):
    t = data["trie"]
    assert t["type"] in (0, 1)
    assert t["valueWidth"] == 1 and "data_32" in t, t["valueWidth"]
    assert t["indexLength"] == len(t["index"])
    assert t["dataLength"] == len(t["data_32"])
    assert all(0 <= x <= 65535 for x in t["index"])
    assert all(0 <= x <= 0xffffffff for x in t["data_32"] + data["ce32s"])
    assert all(0 <= x <= 65535 for x in data["contexts"])
    trie_type = "Fast" if t["type"] == 0 else "Small"
    return f"""RawSearchData {{
        locale: {json.dumps(locale)}, attribute: {json.dumps(attribute)}, bits: {bits},
        header: CodePointTrieHeader {{ high_start: {t['highStart']}, shifted12_high_start: {t['shifted12HighStart']}, index3_null_offset: {t['index3NullOffset']}, data_null_offset: {t['dataNullOffset']}, null_value: {t['nullValue']}, trie_type: TrieType::{trie_type} }},
        index: {arr(t['index'])}, data: {arr(t['data_32'])},
        contexts: {arr(data['contexts'])}, ce32s: {arr(data['ce32s'])}, ces: {arr([x & 0xffffffffffffffff for x in data['ces']])},
    }}"""


def trie_index(t, cp):
    index = t["index"]
    if cp < (0x10000 if t["type"] == 0 else 0x1000):
        return index[cp >> 6] + (cp & 63)
    if cp >= t["highStart"]:
        return len(t["data_32"]) - 2
    i1 = (cp >> 14) + (1020 if t["type"] == 0 else 64)
    i2 = index[i1] + ((cp >> 9) & 31)
    i3 = index[i2]
    pos = (cp >> 4) & 31
    if i3 & 0x8000:
        block = (i3 & 0x7fff) + (pos & ~7) + (pos >> 3)
        pos &= 7
        base = ((index[block] << (2 + 2 * pos)) & 0x30000) | index[block + 1 + pos]
    else:
        base = index[i3 + pos]
    return base + (cp & 15)


def overlay_modern_jamo(data):
    """Restore the exact pinned builder's final 67 modern-Jamo CE32 records.

    ICU genrb removes U1100..11FF from its exported trie (parse.cpp convertTrie).
    buildMappings appends modern L19/V21/T27 records before digit/middle-starter
    processing. For these fixed search exports the modern vector is the final67
    records; all22 vectors are validated by the full source/runtime matrix.
    Copy-on-write affects only private small-trie index/data blocks. Every other
    codepoint is asserted unchanged, including old high/error tail values.
    """
    import copy
    original = copy.deepcopy(data)
    t = data["trie"]
    assert t["type"] == 1 and t["highStart"] > 0x1200
    cps = list(range(0x1100, 0x1113)) + list(range(0x1161, 0x1176)) + list(range(0x11a8, 0x11c3))
    assert len(cps) == 67 and len(data["ce32s"]) >= 67
    modern = dict(zip(cps, data["ce32s"][-67:]))
    old_index, old_values = t["index"][:], t["data_32"][:]
    i1 = 64  # U1000..3FFF in a Small trie.
    i2 = len(t["index"])
    t["index"].extend(old_index[old_index[i1]:old_index[i1] + 32])
    t["index"][i1] = i2
    i3 = len(t["index"])
    t["index"].extend([trie_index(original["trie"], 0x1000 + 16*j) for j in range(32)])
    t["index"][i2 + 8] = i3
    for j in range(32):
        start = 0x1000 + 16*j
        if not any(cp in modern for cp in range(start, start+16)):
            continue
        offset = len(t["data_32"])
        assert offset + 16 <= 65535
        t["data_32"].extend([modern.get(cp, old_values[trie_index(original["trie"], cp)]) for cp in range(start, start+16)])
        t["index"][i3+j] = offset
    t["data_32"].extend(old_values[-2:])
    assert len(t["index"]) < 32768
    t["indexLength"] = len(t["index"])
    t["dataLength"] = len(t["data_32"])
    for cp in range(0x110000):
        actual = t["data_32"][trie_index(t, cp)]
        expected = modern.get(cp, old_values[trie_index(original["trie"], cp)])
        assert actual == expected, (hex(cp), actual, expected)
    assert t["data_32"][-1] == old_values[-1]
    return data


def equality_rules(path):
    root = ET.fromstring(path.read_text())
    rules = root.find(".//collation[@type='search']/cr").text
    rules = "\n".join(line.split("#", 1)[0] for line in rules.splitlines())
    mapping = {}
    for block in rules.split("&")[1:]:
        parts = block.split("=")
        reset = re.sub(r"\s", "", parts[0])
        if not reset or not all(0x1100 <= ord(c) < 0x1200 for c in reset):
            continue
        for part in parts[1:]:
            character = re.sub(r"\s", "", part)
            assert len(character) == 1 and 0x1100 <= ord(character) < 0x1200
            assert character not in mapping
            mapping[character] = reset
    return mapping


def korean_resets(root_source, korean_source):
    assert hashlib.sha256(root_source.read_bytes()).hexdigest() == "af6784292b2ec2576efcfdc5e1073f272edc1265a2dc5286ba9bd16b2d7a1ed9"
    assert hashlib.sha256(korean_source.read_bytes()).hexdigest() == "d4c35933a28bd6712138e92528f4c6fe0e2d150b296dcaaa0fdde10c7ba8ca34"
    root, korean = equality_rules(root_source), equality_rules(korean_source)
    assert len(root) == 43 and len(korean) == 158
    def expand(value):
        return "".join(expand(root[c]) if c in root else c for c in value)
    mapping = {c: expand(value) for c, value in korean.items()}
    assert all(not any(c in mapping for c in value) for value in mapping.values())
    def rust_string(value):
        return '"' + "".join("\\u{" + format(ord(c), "x") + "}" for c in value) + '"'
    return """// @generated from pinned CLDR48.2 root/search and ko/search equality rules.
// SPDX-License-Identifier: Unicode-3.0
// Only the archaic conjoining Jamo removed by the official ICU exporter.
// Replacements expand imported root equality resets; U1197 is transitive.
#[rustfmt::skip]
pub(super) const ERASED_KOREAN: &[(char, &str)] = &[\n""" + "".join("    ('\\u{" + format(ord(c), "x") + "}', " + rust_string(value) + "),\n" for c, value in sorted(mapping.items())) + "];\n"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("archive", type=pathlib.Path)
    ap.add_argument("output", type=pathlib.Path)
    ap.add_argument("--cldr-root", required=True, type=pathlib.Path)
    ap.add_argument("--cldr-ko", required=True, type=pathlib.Path)
    ap.add_argument("--korean-output", required=True, type=pathlib.Path)
    args = ap.parse_args()
    digest = hashlib.sha256(args.archive.read_bytes()).hexdigest()
    assert digest == EXPECTED, digest
    records, rows = [], []
    with zipfile.ZipFile(args.archive) as z:
        names = sorted(x for x in z.namelist() if x.startswith("collation/implicithan/") and "_search" in x)
        for name in names:
            if not name.endswith("_data.toml"):
                continue
            meta = name[:-len("_data.toml")] + "_meta.toml"
            b, m = z.read(name), z.read(meta)
            base = name.rsplit("/", 1)[1][:-len("_data.toml")]
            lang, attribute = base.rsplit("_", 1)
            locale = "und" if lang == "root" else lang.replace("_", "-")
            bits = tomllib.loads(m.decode())["bits"] | 8
            rows.append(raw(overlay_modern_jamo(tomllib.loads(b.decode())), locale, attribute, bits))
            records.extend({"name": n, "sha256": hashlib.sha256(v).hexdigest(), "bytes": len(v)} for n, v in [(name, b), (meta, m)])
        assert len(rows) == 22
        root = tomllib.loads(z.read("collation/implicithan/root_standard_data.toml").decode())
        jamo = tomllib.loads(z.read("collation/implicithan/root_standard_jamo.toml").decode())["ce32s"]
        dia = tomllib.loads(z.read("collation/implicithan/root_standard_dia.toml").decode())["secondaries"]
    result = """// @generated by tools/generate-search-collation-data.py from official ICU 78.1rc.
// SPDX-License-Identifier: Unicode-3.0
// Input SHA256: """ + digest + """
// Safe CodePointTrie::try_new validates every emitted header/index/data combination.
#![allow(clippy::unreadable_literal)]
use super::RawSearchData;
use icu_collections::codepointtrie::{CodePointTrieHeader, TrieType};
#[rustfmt::skip]
pub(super) const SOURCES: &[RawSearchData] = &[
""" + ",\n".join(rows) + "\n];\n"
    result += "#[cfg(test)]\n#[rustfmt::skip]\npub(super) const ROOT_COMPATIBILITY: RawSearchData = " + raw(root) + ";\n"
    result += "#[cfg(test)]\n#[rustfmt::skip]\npub(super) const ROOT_JAMO: &[u32] = " + arr(jamo) + ";\n"
    result += "#[cfg(test)]\n#[rustfmt::skip]\npub(super) const ROOT_DIACRITICS: &[u16] = " + arr(dia) + ";\n"
    hangul = korean_resets(args.cldr_root, args.cldr_ko)
    args.korean_output.parent.mkdir(parents=True, exist_ok=True)
    if args.korean_output.exists() and args.korean_output.read_text() != hangul:
        raise RuntimeError("immutable Korean output differs")
    args.korean_output.write_text(hangul)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    if args.output.exists() and args.output.read_text() != result:
        raise RuntimeError("immutable generated output differs")
    args.output.write_text(result)
    manifest = {
        "archive": str(args.archive), "archive_sha256": digest,
        "generator_sha256": hashlib.sha256(pathlib.Path(__file__).read_bytes()).hexdigest(),
        "output": str(args.output), "output_sha256": hashlib.sha256(result.encode()).hexdigest(),
        "search_entries": len(rows), "input_files": records,
        "scope": "22 actual search/searchjl tailorings; raw trie retains original representation. ICU4X source converter only rebuilds the trie and clears Hangul syllable mappings; runtime handles syllables separately. All22 source vectors restore67 modern Jamo by a checked copy-on-write overlay; only those67 codepoints differ. Korean archaic equality preprocessing is a separately pinned CLDR source rule set.",
    }
    args.output.with_suffix(".manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(json.dumps({k: v for k, v in manifest.items() if k != "input_files"}))


if __name__ == "__main__":
    main()

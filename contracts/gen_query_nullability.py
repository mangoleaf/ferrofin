#!/usr/bin/env python3
"""Generate crates/ferrofin-api/src/query/non_nullable.json.

For every upstream API action, list the query parameters that refuse an empty
value: a NON-nullable value type (`bool enableTotalRecordCount = true`, `int
limit = 20`), anything `[Required]`, and a non-nullable `string` (nullable
reference types are on upstream, so MVC treats it as implicitly required). ASP.NET's `SimpleTypeModelBinder` binds an empty or
whitespace value to null and `CheckModel` then rejects it for exactly these
parameters ("The value '' is invalid.", 400); a nullable parameter (`bool?
isFavorite`) just binds null. The OpenAPI document cannot tell the two apart
(`bool? enableImages = true` also carries `default: true`), so the C# action
signatures are the source.

Routes come from each action's own attributes — the controller's `[Route]`
plus every `[HttpGet|Head|Post|Put|Delete|Patch("…")]`, aliases (`Name = …`)
included — so obsolete actions the OpenAPI document omits (`/Users/{userId}/
Items`) are covered too. Each release's controllers are read with `git show`,
so the output is reproducible; the newer release wins:

    contracts/gen_query_nullability.py ~/dev/3rdparty/jellyfin

Keys are `METHOD /Path/{param}` as upstream writes them; the server compares
them with parameters erased and literals case-folded.
"""
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "crates/ferrofin-api/src/query/non_nullable.json"
TAGS = ["v10.11.8", "v12.1"]  # later wins
VALUE_TYPES = {
    "bool", "byte", "sbyte", "short", "ushort", "int", "uint", "long", "ulong",
    "float", "double", "decimal", "Guid", "DateTime", "DateTimeOffset", "TimeSpan",
}
HTTP = re.compile(r'\[Http(Get|Head|Post|Put|Delete|Patch)(?:\(\s*(?:"([^"]*)")?[^\]]*\))?\]')


def git(repo, *args):
    return subprocess.run(["git", "-C", repo, *args], check=True,
                          capture_output=True, text=True).stdout


def enum_names(repo, tag):
    """Every C# enum declared at `tag` (they are value types too)."""
    out = git(repo, "grep", "-h", "-o", "-E", r"public enum [A-Za-z0-9_]+", tag, "--", "*.cs")
    return {line.split()[-1] for line in out.splitlines()}


def split_params(params):
    depth, current, out = 0, "", []
    for ch in params:
        if ch in "<([":
            depth += 1
        elif ch in ">)]":
            depth -= 1
        if ch == "," and depth == 0:
            out.append(current)
            current = ""
        else:
            current += ch
    if current.strip():
        out.append(current)
    return out


def non_nullable(params, enums):
    names = []
    for param in split_params(params):
        param = " ".join(param.split())
        # Only the LEADING `[...]` blocks are attributes; `[]` after a type
        # name is an array.
        lead = re.match(r"\s*((?:\[[^\]]*\]\s*)*)", param).group(1)
        if "FromQuery" not in lead:
            continue
        decl = param[len(lead):].split("=")[0].strip()
        parts = decl.split()
        if len(parts) < 2:
            continue
        ty, name = parts[-2], parts[-1]
        if ty.endswith("[]") or "<" in ty:
            continue
        # `[Required]` makes any type refuse an empty value; with nullable
        # reference types on (`<Nullable>enable</Nullable>`), so does a
        # non-nullable `string` (MVC's implicit required).
        required = re.search(r"\bRequired\b", lead) is not None
        nonnull_value = not ty.endswith("?") and (ty in VALUE_TYPES or ty in enums)
        nonnull_string = ty == "string"
        if not (required or nonnull_value or nonnull_string):
            continue
        override = re.search(r'FromQuery\s*\(\s*Name\s*=\s*"([^"]+)"', lead)
        names.append(override.group(1) if override else name)
    return names


def join_route(prefix, template):
    if template is None or template == "":
        path = prefix
    elif template.startswith(("~/", "/")):
        path = template.lstrip("~/")
    else:
        path = f"{prefix}/{template}" if prefix else template
    return "/" + path.strip("/")


def actions(repo, tag, enums):
    """(METHOD, path) -> non-nullable query names, for every controller action."""
    table = {}
    files = git(repo, "ls-tree", "-r", "--name-only", tag, "--", "Jellyfin.Api/Controllers").split()
    for path in files:
        if not path.endswith(".cs"):
            continue
        text = git(repo, "show", f"{tag}:{path}")
        # A controller without its own `[Route]` inherits
        # `BaseJellyfinApiController`'s `[Route("[controller]")]`.
        class_match = re.search(r"public\s+(?:sealed\s+|abstract\s+)*class\s+(\w+?)(?:Controller)?\b", text)
        controller = class_match.group(1) if class_match else ""
        prefix_match = re.search(r'\[Route\("([^"]*)"\)\]\s*(?:\[[^\]]*\]\s*)*public\s+(?:sealed\s+|abstract\s+)*class', text)
        prefix = prefix_match.group(1) if prefix_match else "[controller]"
        prefix = prefix.replace("[controller]", controller).strip("/")
        for match in re.finditer(r"public\s+[^\n(;=]*?\s(\w+)\(", text):
            start = match.end()
            depth, i = 1, start
            while depth and i < len(text):
                depth += {"(": 1, ")": -1}.get(text[i], 0)
                i += 1
            if not text[i:i + 40].lstrip().startswith(("{", "=>")):
                continue
            # The attribute lines directly above the declaration.
            lines = text[:match.start()].splitlines()
            attrs = []
            for line in reversed(lines):
                stripped = line.strip()
                if stripped.startswith("[") or stripped.startswith("///") or not stripped:
                    if stripped.startswith("["):
                        attrs.append(stripped)
                    if not stripped and attrs:
                        break
                    continue
                break
            routes = [(m.group(1).upper(), join_route(prefix, m.group(2))) for m in HTTP.finditer(" ".join(attrs))]
            if not routes:
                continue
            names = non_nullable(text[start:i - 1], enums)
            for route in routes:
                table[route] = names
    return table


def main():
    repo = sys.argv[1] if len(sys.argv) > 1 else str(Path.home() / "dev/3rdparty/jellyfin")
    merged = {}
    for tag in TAGS:
        merged.update(actions(repo, tag, enum_names(repo, tag)))
    table = {f"{m} {p}": sorted(set(n), key=str.lower) for (m, p), n in merged.items() if n}
    # The server matches routes by shape (parameters erased, literals
    # case-folded); two routes of one shape must agree.
    shapes = {}
    for (m, p), n in merged.items():
        shape = (m, "/".join("{}" if "{" in s else s.lower() for s in p.split("/")))
        if shape in shapes and sorted(shapes[shape]) != sorted(n):
            sys.exit(f"routes collide on shape {shape}: {shapes[shape]} vs {n}")
        shapes[shape] = n
    OUT.write_text(json.dumps(dict(sorted(table.items())), indent=1) + "\n")
    print(f"{len(table)} routes, {sum(map(len, table.values()))} parameters -> {OUT.relative_to(ROOT)}")


if __name__ == "__main__":
    main()

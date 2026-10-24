#!/usr/bin/env python3
"""List every schema reachable from JSON request bodies in both vendored contracts.

Run from the repository root. Enum constants are independently recorded with:
  dotnet run --project verify/json-binding-oracle -p:JellyfinDirectory=DIR -- \
    --enum-inventory DIR
The schema inventory includes nested/array members and avoids reference cycles.
"""
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def inventory():
    result = {"contracts": [], "body_schemas": [], "enums": [], "numeric_fields": []}
    names, enums, numbers = set(), set(), set()
    for path in sorted((ROOT / "contracts").glob("jellyfin-openapi-*.json")):
        spec = json.loads(path.read_text())
        schemas = spec["components"]["schemas"]
        visited = set()

        def walk(node, owner="", field=""):
            if isinstance(node, list):
                for entry in node:
                    walk(entry, owner, field)
            elif isinstance(node, dict):
                if "$ref" in node:
                    name = node["$ref"].rsplit("/", 1)[-1]
                    if name in schemas and name not in visited:
                        visited.add(name)
                        names.add(name)
                        if "enum" in schemas[name]:
                            enums.add(name)
                        walk(schemas[name], name)
                if node.get("type") in ("integer", "number"):
                    numbers.add((owner, field, node["type"], node.get("format", ""),
                                 node.get("nullable", False)))
                for key, value in node.items():
                    if key == "properties":
                        for member, schema in value.items():
                            walk(schema, owner, member)
                    elif key == "items":
                        walk(value, owner, field + "[]")
                    elif key != "$ref":
                        walk(value, owner, field)

        for operations in spec["paths"].values():
            for operation in operations.values():
                if isinstance(operation, dict) and "requestBody" in operation:
                    walk(operation["requestBody"])
        result["contracts"].append({"file": path.name, "version": spec["info"]["version"]})
    result["body_schemas"] = sorted(names)
    result["enums"] = sorted(enums)
    result["numeric_fields"] = [dict(zip(("schema", "field", "type", "format", "nullable"), item))
                                for item in sorted(numbers)]
    return result


if __name__ == "__main__":
    print(json.dumps(inventory(), indent=2))

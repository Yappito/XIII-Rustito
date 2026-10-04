#!/usr/bin/env python3
"""Rebuild compact research summaries from probe inventories and baseline files.

Run after probe_install.py for both installations. Never launches the game.
objdump is optional, for PE export metadata only. No payloads are written.
"""
import argparse
from collections import Counter, defaultdict
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess

from probe_install import ASSET_EXTENSIONS, package_tables

FOCUS = [
    "system/xiii.u", "system/xidmaps.u", "system/engine.u",
    "system/xidpawn.u", "system/xidcine.u", "system/PC/xiiipersos.u",
    "Maps/Plage00.unr", "Maps/Plage01.unr",
]


def normalized_path(item):
    return item["path"].lower().replace("texturespc/", "textures/").replace("system/pc/", "system/")


def indexed(files, key):
    result = defaultdict(list)
    for item in files:
        result[key(item)].append(item)
    return result


def compare(gog, steam, key):
    a, b = indexed(gog, key), indexed(steam, key)
    result = {"identical": [], "changed": [], "ambiguous": [],
              "only_gog": sorted(a.keys() - b.keys()), "only_steam": sorted(b.keys() - a.keys())}
    for name in sorted(a.keys() & b.keys()):
        if len(a[name]) != 1 or len(b[name]) != 1:
            result["ambiguous"].append(name)
            continue
        left, right = a[name][0], b[name][0]
        state = "identical" if left["sha256"] == right["sha256"] else "changed"
        result[state].append({"gog_path": left["path"], "steam_path": right["path"]})
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gog-root", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, default=Path("docs/evidence"))
    args = parser.parse_args()
    root, out = args.gog_root.resolve(), args.evidence.resolve()
    if out.is_relative_to(root):
        parser.error("evidence directory must be outside the source installation")
    gog = json.loads((out / "gog-inventory.json").read_text(encoding="utf-8"))
    steam = json.loads((out / "steam-inventory.json").read_text(encoding="utf-8"))
    hashes = {f["path"]: f["sha256"] for f in gog["files"]}

    def write(name, value):
        (out / name).write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")

    # Validate inputs before publishing any derived reports.
    focus = {}
    for name in FOCUS:
        data = (root / name).read_bytes()
        if hashlib.sha256(data).hexdigest() != hashes[name]:
            raise ValueError(f"baseline changed: {name}; rerun inventories first")
        package = package_tables(data)
        exports = package.pop("exports_metadata")
        package["text_buffer_size_histogram"] = dict(Counter(
            str(e["serial_size"]) for e in exports if e["class"] == "Core.TextBuffer"))
        focus[name] = package

    file_comparison = compare(gog["files"], steam["files"], normalized_path)
    file_comparison.update(schema=1, match_rule="lowercase paths; TexturesPC -> Textures; system/PC -> system; analytical only")
    write("install-comparison.json", file_comparison)
    assets = lambda d: [f for f in d["files"] if Path(f["path"]).suffix.lower() in ASSET_EXTENSIONS]
    asset_comparison = compare(assets(gog), assets(steam), lambda f: Path(f["path"]).name.lower())
    asset_comparison.update(schema=1, match_rule="unique case-insensitive asset basename; no runtime precedence implied")
    write("asset-comparison.json", asset_comparison)
    write("focus-packages.json", focus)

    known = {Path(f["path"]).stem.lower() for f in gog["files"] if "package" in f}
    missing = defaultdict(list)
    for f in gog["files"]:
        for dependency in f.get("package", {}).get("imported_packages", []):
            if dependency.lower() not in known:
                missing[dependency].append(f["path"])
    write("unresolved-package-stems.json", missing)

    if shutil.which("objdump"):
        dlls = {}
        for p in sorted((root / "system").glob("*.dll")):
            if hashlib.sha256(p.read_bytes()).hexdigest() != hashes[p.relative_to(root).as_posix()]:
                raise ValueError(f"baseline changed: {p.name}; rerun inventories first")
            result = subprocess.run(["objdump", "-p", str(p)], capture_output=True, text=True, check=True)
            table = result.stdout.split("[Ordinal/Name Pointer] Table", 1)
            symbols = []
            if len(table) == 2:
                for line in table[1].splitlines():
                    if line.startswith("\t["):
                        symbols.append(line.split()[-1])
            methods = sorted({s for s in symbols if re.match(r"\?exec\w+@", s)})
            dlls[p.name] = {"named_exports": len(set(symbols)),
                           "exec_method_exports": len(methods), "examples": methods[:5]}
        write("native-dll-summary.json", {
            "schema": 2,
            "method": "objdump PE named-export table; unique decorated ?exec method symbols only; not a complete native operation inventory",
            "dlls": dlls,
        })
    else:
        print("objdump unavailable; existing native DLL summary was not regenerated")

    print("Asset comparison:", {k: len(v) for k, v in asset_comparison.items() if isinstance(v, list)})
    print("Unresolved top-level package stems:", len(missing))
    print("Focus packages:", len(focus))


if __name__ == "__main__":
    main()

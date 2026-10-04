#!/usr/bin/env python3
"""Read-only XIII research probe; metadata/table validation, NOT an asset loader.

Python 3.10+, standard library only. Never launches or changes the game.
Reports hashes, package headers and table metadata, not proprietary payloads.
Only the locally observed UE package version 100 dialect is supported.
"""
from __future__ import annotations

import argparse
from collections import Counter, defaultdict
import hashlib
import json
from pathlib import Path
import struct

MAGIC = b"\xc1\x83\x2a\x9e"
ASSET_EXTENSIONS = {".u", ".unr", ".utx", ".usx", ".uax", ".ukx", ".hxc", ".hsc", ".bik"}
MAX_ITEMS = 1_000_000


class FormatError(ValueError):
    pass


class Reader:
    def __init__(self, data: bytes, offset: int = 0):
        self.data = data
        self.pos = offset

    def take(self, size: int) -> bytes:
        if size < 0 or self.pos < 0 or self.pos + size > len(self.data):
            raise FormatError(f"out of bounds at {self.pos}, requested {size}")
        result = self.data[self.pos:self.pos + size]
        self.pos += size
        return result

    def unpack(self, fmt: str):
        return struct.unpack(fmt, self.take(struct.calcsize(fmt)))

    def byte(self) -> int:
        return self.take(1)[0]

    def compact(self) -> int:
        first = self.byte()
        value = first & 0x3f
        more = first & 0x40
        shift = 6
        for index in range(1, 5):
            if not more:
                break
            b = self.byte()
            if index == 4:
                if b & 0xe0:
                    raise FormatError("compact index exceeds 32-bit magnitude")
                value |= b << shift
                more = False
            else:
                value |= (b & 0x7f) << shift
                more = b & 0x80
            shift += 7
        if value > (0x80000000 if first & 0x80 else 0x7fffffff):
            raise FormatError("compact index outside signed i32")
        return -value if first & 0x80 else value

    def string(self) -> str:
        length = self.compact()
        if abs(length) > MAX_ITEMS:
            raise FormatError(f"oversized string: {length}")
        if not length:
            return ""
        raw = self.take(abs(length) * (2 if length < 0 else 1))
        terminator = b"\x00\x00" if length < 0 else b"\x00"
        if not raw.endswith(terminator):
            raise FormatError("unterminated name")
        return raw[:-len(terminator)].decode("utf-16-le" if length < 0 else "latin-1")


def package_tables(data: bytes) -> dict:
    r = Reader(data)
    fields = r.unpack("<IHHIiiiiii")
    magic, version, licensee, flags, nc, no, ec, eo, ic, io = fields
    if magic != 0x9e2a83c1 or version != 100:
        raise FormatError(f"unsupported package {magic:08x} {version}/{licensee}")
    for label, count, offset in [("names", nc, no), ("exports", ec, eo), ("imports", ic, io)]:
        if not 0 <= count <= MAX_ITEMS or not 0 <= offset <= len(data):
            raise FormatError(f"invalid {label} table: {count} at {offset}")
    names = []
    r.pos = no
    for _ in range(nc):
        names.append(r.string())
        r.take(4)  # Name flags.
    names_end = r.pos

    def name(index):
        if not 0 <= index < nc:
            raise FormatError(f"bad name index {index}")
        return names[index]

    imports = []
    r.pos = io
    for _ in range(ic):
        cp, cn = name(r.compact()), name(r.compact())
        outer, = r.unpack("<i")
        imports.append({"class_package": cp, "class": cn, "outer": outer, "name": name(r.compact())})
    imports_end = r.pos
    exports = []
    r.pos = eo
    for _ in range(ec):
        cls, super_ref = r.compact(), r.compact()
        outer, = r.unpack("<i")
        obj_name = name(r.compact())
        obj_flags, = r.unpack("<I")
        size = r.compact()
        offset = r.compact() if size else 0
        if size < 0 or offset < 0 or offset + size > len(data):
            raise FormatError(f"invalid export span for {obj_name}: {offset}+{size}")
        exports.append({"name": obj_name, "class_ref": cls, "super_ref": super_ref,
                        "outer": outer, "flags": obj_flags, "serial_size": size, "serial_offset": offset})
    exports_end = r.pos

    def obj(ref):
        if not -ic <= ref <= ec:
            raise FormatError(f"bad object reference {ref}")
        return exports[ref - 1] if ref > 0 else imports[-ref - 1] if ref < 0 else None

    def path(ref):
        parts, seen = [], set()
        while ref:
            if ref in seen or len(seen) > 256:
                raise FormatError("outer reference cycle/depth exceeded")
            seen.add(ref)
            item = obj(ref)
            parts.append(item["name"])
            ref = item["outer"]
        return ".".join(reversed(parts))

    classes = Counter()
    zero = Counter()
    for e in exports:
        obj(e["super_ref"])
        cls = path(e["class_ref"]) if e["class_ref"] else "Core.Class"
        e["class"] = cls
        classes[cls] += 1
        if not e["serial_size"]:
            zero[cls] += 1
    for ref in list(range(-ic, 0)) + list(range(1, ec + 1)):
        path(ref)
    roots = sorted({i["name"] for i in imports if i["outer"] == 0 and i["class"] == "Package"})
    return {
        "version": version, "licensee_version": licensee, "package_flags": flags,
        "names": nc, "imports": ic, "exports": ec,
        "table_spans": {"names": [no, names_end], "imports": [io, imports_end], "exports": [eo, exports_end]},
        "table_validation": "passed", "export_classes": dict(sorted(classes.items())),
        "zero_size_exports": dict(sorted(zero.items())), "imported_packages": roots,
        "exports_metadata": exports,
    }


def inspect(root: Path, label: str, include_exports: bool = False) -> dict:
    files = []
    extension_totals = defaultdict(lambda: {"files": 0, "bytes": 0})
    for p in sorted(root.rglob("*")):
        # Avoid following source-install symlinks outside the supplied root.
        if not p.is_file() or p.is_symlink():
            continue
        rel = p.relative_to(root).as_posix()
        if any(part.lower() in {"save", "saves", "profiles", "cache", "logs"} for part in p.relative_to(root).parts):
            continue
        ext = p.suffix.lower()
        size = p.stat().st_size
        extension_totals[ext]["files"] += 1
        extension_totals[ext]["bytes"] += size
        if ext not in ASSET_EXTENSIONS and ext not in {".exe", ".dll", ".ini"}:
            continue
        digest = hashlib.sha256()
        with p.open("rb") as f:
            prefix = f.read(40)
            digest.update(prefix)
            for chunk in iter(lambda: f.read(1024 * 1024), b""):
                digest.update(chunk)
        item = {"path": rel, "bytes": size, "sha256": digest.hexdigest()}
        if prefix[:4] == MAGIC:
            try:
                tables = package_tables(p.read_bytes())
                if not include_exports:
                    tables.pop("exports_metadata")
                item["package"] = tables
            except (FormatError, UnicodeError, struct.error) as e:
                item["package_error"] = str(e)
        files.append(item)
    return {"schema": 1, "label": label, "scope": "assets, binaries and INI hashes; saves excluded",
            "extensions": dict(sorted(extension_totals.items())), "files": files}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--label", default="local")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--exports", action="store_true", help="Include object metadata (no payloads).")
    args = parser.parse_args()
    root = args.root.resolve()
    if not root.is_dir():
        parser.error("root must be an existing installation directory")
    if args.out.resolve().is_relative_to(root):
        parser.error("output must be outside the source installation")
    result = inspect(root, args.label, args.exports)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    packages = sum("package" in f for f in result["files"])
    errors = sum("package_error" in f for f in result["files"])
    print(f"{args.label}: {len(result['files'])} hashed files; {packages} package tables parsed; {errors} package errors")
    return 1 if errors else 0


if __name__ == "__main__":
    raise SystemExit(main())

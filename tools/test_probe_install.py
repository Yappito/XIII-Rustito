"""Synthetic boundary checks; no retail data is required."""
import struct
import unittest

from probe_install import FormatError, Reader, package_tables
from build_evidence import compare


class ReaderTests(unittest.TestCase):
    def test_compact_boundaries(self):
        cases = [
            (b"\x00", 0), (b"\x3f", 63), (b"\x40\x01", 64),
            (b"\xbf", -63), (b"\xc0\x01", -64),
            (b"\x40\x80\x01", 8192),
            (b"\x7f\xff\xff\xff\x0f", 2**31 - 1),
            (b"\xc0\x80\x80\x80\x10", -(2**31)),
        ]
        for raw, expected in cases:
            with self.subTest(raw=raw):
                reader = Reader(raw)
                self.assertEqual(reader.compact(), expected)
                self.assertEqual(reader.pos, len(raw))

    def test_invalid_compact_indices(self):
        for raw in [b"", b"\x40", b"\x40\x80\x80\x80\x20", b"\x40\x80\x80\x80\x10"]:
            with self.subTest(raw=raw), self.assertRaises(FormatError):
                Reader(raw).compact()

    def test_name_encodings(self):
        self.assertEqual(Reader(b"\x05None\0").string(), "None")
        self.assertEqual(Reader(b"\x82\xe9\x00\x00\x00").string(), "é")

    def test_bad_string_bounds_and_terminator(self):
        for raw in [b"\x04abc", b"\x03abc", b"\x82\xe9\x00"]:
            with self.subTest(raw=raw), self.assertRaises(FormatError):
                Reader(raw).string()

    def test_unsupported_or_truncated_package(self):
        for raw in [b"", bytes(35), bytes(36), struct.pack("<IHHIiiiiii", 0x9e2a83c1, 128, 29, 0, 0, 36, 0, 36, 0, 36)]:
            with self.subTest(raw=raw), self.assertRaises(FormatError):
                package_tables(raw)

    def test_out_of_range_object_reference(self):
        # Minimal synthetic table: one name, one import whose outer is import #2.
        names = b"\x05None\x00" + bytes(4)
        imports = b"\x00\x00" + struct.pack("<i", -2) + b"\x00"
        header = struct.pack("<IHHIiiiiii", 0x9e2a83c1, 100, 58, 0, 1, 36, 0, 36, 1, 36 + len(names))
        with self.assertRaisesRegex(FormatError, "bad object reference"):
            package_tables(header + names + imports)

    def test_outer_cycle(self):
        names = b"\x05None\x00" + bytes(4)
        imports = b"\x00\x00" + struct.pack("<i", -1) + b"\x00"
        header = struct.pack("<IHHIiiiiii", 0x9e2a83c1, 100, 58, 0, 1, 36, 0, 36, 1, 36 + len(names))
        with self.assertRaisesRegex(FormatError, "cycle"):
            package_tables(header + names + imports)

    def test_comparison_does_not_hide_duplicates(self):
        a = [{"path": "one/x.u", "sha256": "a"}, {"path": "two/x.u", "sha256": "b"}]
        b = [{"path": "x.u", "sha256": "a"}]
        result = compare(a, b, lambda f: f["path"].split("/")[-1])
        self.assertEqual(result["ambiguous"], ["x.u"])
        self.assertEqual(result["identical"], [])


if __name__ == "__main__":
    unittest.main()

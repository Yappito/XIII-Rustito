# xiii-locale

Reader for the UE2 `.int` localisation files shipped next to XIII's packages
(`XIII_Game/system/*.int` for English, `*.frt`, `*.det`, `*.est`, `*.itt` for
French/German/Spanish/Italian; the other language files are uppercase stems,
e.g. `XIDInterf.frt`).

Two layers:

- `LocalizationFile::parse(relative, stem, language, &[u8])` is dependency-free
  and filesystem-free. It detects the encoding, parses sections and
  `Key=Value` pairs, and records every malformed line with a 1-based line
  number.
- `Localizer::from_installation(&Installation)` scans the bounded inventory
  that `xiii-install` already produced, parses every file whose extension is a
  known language code, reads `[Engine.Engine] Language=` from `Default.ini`
  (then `XIII.ini`) and answers `get(package, section, key)` with fallback to
  `int`.

## Measured corpus (GOG, 2026-10-05)

`xiii-tool locale coverage XIII_Game`:

| | |
|---|---|
| Files | 216: `int` 48, `frt`/`det`/`est`/`itt` 42 each |
| Encodings | Windows-1252 187, ASCII/UTF-8 29; **no** UTF-16 and no BOM |
| Active language | `int` from `Default.ini` `[Engine.Engine] Language=` |
| Distinct sections | 4674 (14 repeated section headers merged) |
| Distinct entries | 17540 |
| Overwritten duplicate keys | 242 |
| Hard errors | 0 |
| Tolerated warnings | 7 (6 missing `=`, 1 unterminated quote, all Italian) |

Line endings are `\r\n` throughout, and no bare `\n` or lone `\r` separates
records. High bytes (`0x84 0x85 0x92 0x93 0x94 0x96 0x9c ...`) are Windows-1252
punctuation: `0x85` decodes to `…`, not the C1 NEL control. Values take the rest
of the line verbatim, so trailing spaces are significant (`(CHAT) `); leading
spaces/tabs after `=` are skipped. One outer pair of `"` quotes is removed;
inner quotes survive because dialogue values embed them
(`Speakers=((PawnName="Pam",...))`). Backslash escapes are kept verbatim (only
`\n` occurs). Duplicate keys are last-wins, matching Unreal's one-value-per-key
config cache.

## `localized` class defaults

A property declared `localized` in UnrealScript loads its default from the
`.int` file of its class's package: section = class name, key = property name
(array elements use `Property[index]`, 0-based). For example
`xidinterf.int [XIIIMenu] LoadGameText=Load game` is
`Localizer::class_property("XIDInterf", "XIIIMenu", "LoadGameText", None)`.
Map actors' values live in the map package's file instead, e.g.
`plage01.int [Plage01CahuteKeyPick0] PickupMessage=First-aid post key.`.

## Tests

- 24 unit tests: encodings (UTF-16 LE/BE BOM, UTF-8 BOM, Windows-1252, odd
  UTF-16, lone surrogate), quoting, leading/trailing spaces, `\n` escapes,
  duplicate keys/repeated sections, malformed lines, keys outside a section,
  literal lone `\r`, language fallback and `class_property`.
- `tests/local_corpus.rs` (opt-in): with `XIII_GOG_DIR` set, asserts the 216
  files / 5 languages, that every file decodes with zero hard errors, the 7
  measured warnings, and a list of known keys. Without the variable it prints
  `SKIPPED`.

Nothing in this crate writes to an installation.

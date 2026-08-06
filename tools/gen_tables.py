#!/usr/bin/env python3
"""Generate server/src/tables.rs from the upstream VS Code extension's TypeScript tables.

The tables are the whole correctness story of this project: 22 tables x 256 entries =
5632 values, where a single wrong digit silently corrupts data. Rather than transcribe
them, we parse them out of the upstream source so the port is provably byte-identical,
and so the derivation can be re-run and diffed at any time.

Upstream: https://github.com/CoderAllan/vscode-ebcdicconverter (MIT, (c) 2020 Allan Simonsev)

Usage:
    python3 tools/gen_tables.py                 # fetch from GitHub, write tables.rs
    python3 tools/gen_tables.py --check         # verify committed tables.rs is current
    python3 tools/gen_tables.py --from-dir DIR  # parse local .ts files instead of fetching
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys
import urllib.request

UPSTREAM_RAW = "https://raw.githubusercontent.com/CoderAllan/vscode-ebcdicconverter/master/src"

# Upstream's supported codepages, in upstream's declaration order.
CODEPAGES = [
    "0037", "0273", "0277", "0278", "0280",
    "0284", "0285", "0297", "0500", "0871", "1047",
]

REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT_PATH = REPO_ROOT / "server" / "src" / "tables.rs"

# Deliberate corrections applied to upstream's tables, as (codepage, index, expected, fixed).
# Each entry asserts what upstream currently has, so if upstream changes, regeneration fails
# loudly instead of silently dropping or duplicating a fix.
#
# 0277/0x0D: upstream maps EBCDIC CR to "" while all ten other codepages map it to "\r",
# which silently deletes every carriage return in a cp0277 conversion. Almost certainly a
# literal CR stripped from the source by an editor rather than an intentional mapping.
PATCHES = [("0277", 0x0D, "", "\r")]


def fetch(name: str, from_dir: str | None) -> str:
    if from_dir:
        return (pathlib.Path(from_dir) / name).read_text(encoding="utf-8")
    url = f"{UPSTREAM_RAW}/{name}"
    print(f"fetching {url}", file=sys.stderr)
    with urllib.request.urlopen(url) as response:  # noqa: S310 - fixed https URL
        return response.read().decode("utf-8")


_NUMBER = re.compile(r"0[xX][0-9a-fA-F]+|\d+")


def _consume_string(source: str, start: int) -> tuple[str, int]:
    """Read the JS string literal beginning at `start`; return (raw body, index after it)."""
    quote = source[start]
    index = start + 1
    body: list[str] = []
    while index < len(source):
        char = source[index]
        if char == "\\":
            body.append(source[index:index + 2])
            index += 2
        elif char == quote:
            return "".join(body), index + 1
        else:
            body.append(char)
            index += 1
    raise SystemExit(f"error: unterminated string literal at offset {start}")


def scan_array(source: str, identifier: str) -> list[str | int]:
    """Tokenize the elements of `identifier: <type> = [ ... ]`.

    Walks the literal character by character rather than pattern-matching it. This is
    required, not fastidious: the tables contain '[' and ']' as *data* (EBCDIC 0xBA/0xBB),
    so bracket counting terminates the array early, and they contain "'" and '"' and '\\',
    so a regex for string literals mis-aligns on the quote characters inside them.
    """
    anchor = re.search(rf"\b{re.escape(identifier)}\s*:\s*[^=]+=\s*\[", source)
    if not anchor:
        raise SystemExit(f"error: could not find declaration for {identifier}")

    tokens: list[str | int] = []
    index = anchor.end()
    while index < len(source):
        char = source[index]
        if char in "'\"":
            raw, index = _consume_string(source, index)
            tokens.append(decode_js_string(raw))
        elif char == "]":
            return tokens
        elif char.isdigit():
            match = _NUMBER.match(source, index)
            if not match:
                raise SystemExit(f"error: bad numeric token in {identifier} at {index}")
            tokens.append(int(match.group(0), 0))
            index = match.end()
        elif char in " \t\r\n,":
            index += 1
        else:
            raise SystemExit(
                f"error: unexpected character {char!r} in {identifier} at offset {index}"
            )
    raise SystemExit(f"error: unterminated array literal for {identifier}")


def parse_number_table(source: str, identifier: str) -> list[int]:
    """Parse a `number[]` table (ASCII -> EBCDIC): 256 byte values."""
    values = scan_array(source, identifier)
    validate_length(identifier, values)
    for position, value in enumerate(values):
        if not isinstance(value, int) or not 0 <= value <= 0xFF:
            raise SystemExit(f"error: {identifier}[{position}] is not a byte: {value!r}")
    return values  # type: ignore[return-value]


_SIMPLE_ESCAPES = {
    "0": "\x00", "b": "\b", "t": "\t", "n": "\n",
    "v": "\v", "f": "\f", "r": "\r", "'": "'", '"': '"', "\\": "\\",
}


def decode_js_string(raw: str) -> str:
    """Decode a JS string literal body (\\xNN, \\uNNNN, and simple escapes)."""
    out: list[str] = []
    index = 0
    while index < len(raw):
        char = raw[index]
        if char != "\\":
            out.append(char)
            index += 1
            continue
        marker = raw[index + 1]
        if marker == "x":
            out.append(chr(int(raw[index + 2:index + 4], 16)))
            index += 4
        elif marker == "u":
            out.append(chr(int(raw[index + 2:index + 6], 16)))
            index += 6
        elif marker in _SIMPLE_ESCAPES:
            out.append(_SIMPLE_ESCAPES[marker])
            index += 2
        else:
            raise SystemExit(f"error: unsupported JS escape \\{marker}")
    return "".join(out)


def parse_string_table(source: str, identifier: str) -> list[str]:
    """Parse a `string[]` table (EBCDIC -> ASCII): 256 strings.

    Entries are normally one character, but upstream has at least one empty entry
    (codepage 0277 byte 0x0D), so entries are kept as strings to preserve that exactly.
    """
    values = scan_array(source, identifier)
    validate_length(identifier, values)
    for position, value in enumerate(values):
        if not isinstance(value, str):
            raise SystemExit(f"error: {identifier}[{position}] is not a string: {value!r}")
        if len(value) > 1:
            raise SystemExit(
                f"error: {identifier}[{position}] decoded to {len(value)} chars ({value!r})"
            )
    return values  # type: ignore[return-value]


def apply_patches(ebcdic_to_ascii: dict[str, list[str]]) -> None:
    """Apply PATCHES, refusing to proceed if upstream no longer matches expectations."""
    for codepage, index, upstream_value, fixed_value in PATCHES:
        actual = ebcdic_to_ascii[codepage][index]
        if actual != upstream_value:
            raise SystemExit(
                f"error: patch for codepage {codepage} byte {index:#04x} expected upstream "
                f"{upstream_value!r} but found {actual!r}. Upstream changed -- re-evaluate "
                f"whether this correction is still needed, then update PATCHES."
            )
        ebcdic_to_ascii[codepage][index] = fixed_value
        print(f"patched {codepage}[{index:#04x}]: {upstream_value!r} -> {fixed_value!r}",
              file=sys.stderr)


def report_unexpected_anomalies(ebcdic_to_ascii: dict[str, list[str]]) -> None:
    """Fail if any entry is still not exactly one character after patching.

    A short entry deletes a byte on conversion, which is the failure mode that motivated
    PATCHES in the first place. Anything new should be reviewed, not silently shipped.
    """
    anomalies = [
        (codepage, index, value)
        for codepage, table in ebcdic_to_ascii.items()
        for index, value in enumerate(table)
        if len(value) != 1
    ]
    if anomalies:
        for codepage, index, value in anomalies:
            print(f"error: {codepage}[{index:#04x}] = {value!r} is not one character",
                  file=sys.stderr)
        raise SystemExit(
            "error: unreviewed table anomalies; add a PATCHES entry or widen the check"
        )


def validate_length(identifier: str, values: list) -> None:
    if len(values) != 256:
        raise SystemExit(f"error: {identifier} has {len(values)} entries, expected 256")


def rust_str(value: str) -> str:
    """Render a table entry as a Rust string literal.

    Entries are strings rather than chars because upstream has an empty entry
    (codepage 0277 byte 0x0D); `&str` reproduces it without a special case.
    """
    out = ['"']
    for char in value:
        code = ord(char)
        if char == "\\":
            out.append(r"\\")
        elif char == '"':
            out.append(r"\"")
        elif 0x20 <= code <= 0x7E:
            out.append(char)
        else:
            out.append(f"\\u{{{code:x}}}")
    out.append('"')
    return "".join(out)


def format_table(name: str, type_: str, entries: list[str], per_row: int) -> str:
    # rustfmt::skip keeps the fixed-width grid intact. Without it `cargo fmt` rewrites the
    # layout, which then disagrees with `gen_tables.py --check` -- the two CI gates would
    # permanently contradict each other, and the grid is far easier to eyeball against
    # upstream than a reflowed list.
    lines = ["#[rustfmt::skip]", f"pub static {name}: [{type_}; 256] = ["]
    for offset in range(0, 256, per_row):
        row = ", ".join(entries[offset:offset + per_row])
        lines.append(f"    {row},")
    lines.append("];")
    return "\n".join(lines)


def render(ebcdic_to_ascii: dict[str, list[str]], ascii_to_ebcdic: dict[str, list[int]]) -> str:
    out: list[str] = [
        "// @generated by tools/gen_tables.py -- DO NOT EDIT BY HAND.",
        "//",
        "// Conversion tables derived from the upstream VS Code extension:",
        "//   https://github.com/CoderAllan/vscode-ebcdicconverter",
        "//   MIT License, Copyright (c) 2020 Allan Simonsev",
        "//",
        "// Regenerate with: python3 tools/gen_tables.py",
        "",
        "/// A single codepage: EBCDIC byte -> text, and Unicode scalar -> EBCDIC byte.",
        "#[derive(Debug)]",
        "pub struct Codepage {",
        "    /// Upstream codepage identifier, e.g. \"0037\".",
        "    pub id: &'static str,",
        "    /// Indexed by EBCDIC byte, yielding the text upstream produces.",
        "    ///",
        "    /// Entries are `&str` rather than `char` because upstream codepage 0277 maps",
        "    /// byte 0x0D to the empty string; see CP0277_CR_IS_EMPTY below.",
        "    pub to_ascii: &'static [&'static str; 256],",
        "    /// Indexed by Unicode scalar (0..=0xFF), yielding the EBCDIC byte.",
        "    pub to_ebcdic: &'static [u8; 256],",
        "}",
        "",
        f"/// All {len(CODEPAGES)} codepages upstream supports, in upstream's declaration order.",
        "#[rustfmt::skip]",
        f"pub static CODEPAGES: [Codepage; {len(CODEPAGES)}] = [",
    ]
    for codepage in CODEPAGES:
        out.append(
            f'    Codepage {{ id: "{codepage}", '
            f"to_ascii: &EBCDIC_{codepage}_TO_ASCII, "
            f"to_ebcdic: &ASCII_TO_EBCDIC_{codepage} }},"
        )
    out.append("];")
    out.append("")

    out.extend([
        "/// Corrections this project applies to upstream's tables, as",
        "/// (codepage, byte, upstream value, corrected value). Exposed so tests can assert",
        "/// both that the fix is present and that nothing else silently diverges.",
        "///",
        "/// Consumed only by tests, hence the allow: it documents intent in the shipped source",
        "/// and gives the test suite something to assert against.",
        "#[allow(dead_code)]",
        "#[rustfmt::skip]",
        f"pub static PATCHES: [(&str, u8, &str, &str); {len(PATCHES)}] = [",
    ])
    for codepage, index, upstream_value, fixed_value in PATCHES:
        out.append(
            f'    ("{codepage}", 0x{index:02x}, '
            f"{rust_str(upstream_value)}, {rust_str(fixed_value)}),"
        )
    out.extend(["];", ""])

    for codepage in CODEPAGES:
        entries = [rust_str(value) for value in ebcdic_to_ascii[codepage]]
        out.append(f"/// EBCDIC codepage {codepage} -> text, indexed by EBCDIC byte.")
        out.append(format_table(f"EBCDIC_{codepage}_TO_ASCII", "&str", entries, 8))
        out.append("")

        bytes_ = [f"0x{value:02x}" for value in ascii_to_ebcdic[codepage]]
        out.append(f"/// Unicode (0..=0xFF) -> EBCDIC codepage {codepage}, indexed by scalar.")
        out.append(format_table(f"ASCII_TO_EBCDIC_{codepage}", "u8", bytes_, 16))
        out.append("")

    return "\n".join(out)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true",
                        help="verify the committed tables.rs matches upstream; exit 1 if stale")
    parser.add_argument("--from-dir",
                        help="read EbcdicToAscii.ts / AsciiToEbcdic.ts from this directory")
    args = parser.parse_args()

    ebcdic_source = fetch("EbcdicToAscii.ts", args.from_dir)
    ascii_source = fetch("AsciiToEbcdic.ts", args.from_dir)

    ebcdic_to_ascii = {
        codepage: parse_string_table(ebcdic_source, f"ebcdic{codepage}ToAsciiMapping")
        for codepage in CODEPAGES
    }
    ascii_to_ebcdic = {
        codepage: parse_number_table(ascii_source, f"asciiToEbcdic{codepage}Mapping")
        for codepage in CODEPAGES
    }

    apply_patches(ebcdic_to_ascii)
    report_unexpected_anomalies(ebcdic_to_ascii)

    generated = render(ebcdic_to_ascii, ascii_to_ebcdic)

    if args.check:
        if not OUT_PATH.exists():
            print(f"error: {OUT_PATH} does not exist; run gen_tables.py", file=sys.stderr)
            return 1
        if OUT_PATH.read_text(encoding="utf-8") != generated:
            print(f"error: {OUT_PATH} is stale; re-run gen_tables.py", file=sys.stderr)
            return 1
        print(f"ok: {OUT_PATH} matches upstream")
        return 0

    OUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    OUT_PATH.write_text(generated, encoding="utf-8")
    print(f"wrote {OUT_PATH} ({len(CODEPAGES)} codepages, {len(CODEPAGES) * 512} values)",
          file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())

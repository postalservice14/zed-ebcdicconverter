//! Character-level conversion between ASCII/Unicode and EBCDIC.
//!
//! These functions reproduce `Convert.ts` from the upstream VS Code extension exactly. The
//! non-obvious rules are called out at each site; they look like bugs and are not.

use crate::tables::Codepage;

/// Which way a conversion runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// EBCDIC bytes in, readable text out.
    EbcdicToAscii,
    /// Text in, EBCDIC out (as characters whose scalar equals the EBCDIC byte).
    AsciiToEbcdic,
}

impl Direction {
    /// The action title, matching upstream's command titles verbatim.
    pub fn title(self, codepage: &str) -> String {
        match self {
            Direction::EbcdicToAscii => format!("Convert Ebcdic{codepage} to Ascii"),
            Direction::AsciiToEbcdic => format!("Convert Ascii to Ebcdic{codepage}"),
        }
    }

    /// Stable identifier used in code action `data` payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::EbcdicToAscii => "ebcdicToAscii",
            Direction::AsciiToEbcdic => "asciiToEbcdic",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "ebcdicToAscii" => Some(Direction::EbcdicToAscii),
            "asciiToEbcdic" => Some(Direction::AsciiToEbcdic),
            _ => None,
        }
    }
}

/// Convert raw EBCDIC bytes to text.
///
/// Every byte maps through the table; nothing is filtered or dropped.
pub fn ebcdic_to_ascii(bytes: &[u8], codepage: &Codepage) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &byte in bytes {
        out.push_str(codepage.to_ascii[byte as usize]);
    }
    out
}

/// Convert editor text that is *holding* EBCDIC values to readable text.
///
/// Used when there are no bytes to read: an unsaved buffer, or a selection. Upstream's
/// equivalent path indexes its table with `charCodeAt(i)`, i.e. per character, not per UTF-8
/// byte -- so this must iterate `chars()`. Feeding `as_bytes()` through the table instead
/// would convert every non-ASCII character twice.
///
/// Deliberate deviation: upstream indexes a 256-entry array with the raw char code, so any
/// character above U+00FF reads `undefined` and JS string concatenation appends the literal
/// text "undefined". Such characters are passed through unchanged here instead -- they cannot
/// be EBCDIC input, and preserving them is the least destructive option. This is also what
/// U+FFFD from an already-mangled file will hit.
pub fn ebcdic_text_to_ascii(text: &str, codepage: &Codepage) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match u8::try_from(u32::from(character)) {
            Ok(byte) => out.push_str(codepage.to_ascii[byte as usize]),
            Err(_) => out.push(character),
        }
    }
    out
}

/// Convert text to EBCDIC, yielding characters whose scalar values are the EBCDIC bytes.
///
/// Upstream produces its output with `String.fromCharCode(mapping[c])`, so the result is a
/// string of U+0000..U+00FF characters rather than raw bytes. `char::from(u8)` matches that,
/// and keeps the result valid UTF-8 so it can be sent back as an LSP text edit.
pub fn ascii_to_ebcdic(text: &str, codepage: &Codepage) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        // Upstream skips CR entirely rather than translating it: converting both CR and LF
        // corrupts line endings, since EBCDIC newline handling differs. Dropping CR means
        // CRLF input yields a single EBCDIC newline.
        if character == '\r' {
            continue;
        }
        match u8::try_from(u32::from(character)) {
            Ok(byte) => out.push(char::from(codepage.to_ebcdic[byte as usize])),
            // Upstream indexes a 256-entry array with the raw char code, so anything above
            // U+00FF reads `undefined`; `String.fromCharCode(NaN)` then yields U+0000.
            Err(_) => out.push('\0'),
        }
    }
    out
}

/// The input to a conversion: raw bytes off disk, or text as the editor holds it.
///
/// Keeping these apart is the crux of the design. A real EBCDIC file is not valid UTF-8, so
/// by the time it reaches the editor buffer the invalid sequences have become U+FFFD and the
/// original bytes are gone. Whole-file EBCDIC decoding must therefore read from disk, exactly
/// as upstream does; selections and unsaved buffers have no bytes to read and use the text.
#[derive(Debug)]
pub enum Source<'a> {
    /// Raw bytes read from the file on disk.
    Bytes(Vec<u8>),
    /// Text as the editor currently holds it.
    Text(&'a str),
}

/// Convert `source` in `direction` using `codepage`.
pub fn convert(source: &Source<'_>, direction: Direction, codepage: &Codepage) -> String {
    match (direction, source) {
        (Direction::EbcdicToAscii, Source::Bytes(bytes)) => ebcdic_to_ascii(bytes, codepage),
        (Direction::EbcdicToAscii, Source::Text(text)) => ebcdic_text_to_ascii(text, codepage),
        // Encoding always starts from text: the characters the user can see are the input,
        // so there is no byte-level path to take here.
        (Direction::AsciiToEbcdic, Source::Bytes(bytes)) => {
            ascii_to_ebcdic(&String::from_utf8_lossy(bytes), codepage)
        }
        (Direction::AsciiToEbcdic, Source::Text(text)) => ascii_to_ebcdic(text, codepage),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables;

    fn codepage(id: &str) -> &'static tables::Codepage {
        tables::CODEPAGES
            .iter()
            .find(|c| c.id == id)
            .expect("known codepage")
    }

    #[test]
    fn all_eleven_codepages_are_present() {
        let ids: Vec<&str> = tables::CODEPAGES.iter().map(|c| c.id).collect();
        assert_eq!(
            ids,
            [
                "0037", "0273", "0277", "0278", "0280", "0284", "0285", "0297", "0500", "0871",
                "1047"
            ]
        );
    }

    #[test]
    fn ebcdic_0037_decodes_known_bytes() {
        // Spot values from IBM codepage 037.
        let cp = codepage("0037");
        assert_eq!(
            ebcdic_to_ascii(&[0xC8, 0xC5, 0xD3, 0xD3, 0xD6], cp),
            "HELLO"
        );
        assert_eq!(ebcdic_to_ascii(&[0x40], cp), " ");
        assert_eq!(ebcdic_to_ascii(&[0xF0, 0xF9], cp), "09");
        assert_eq!(ebcdic_to_ascii(&[0x81, 0xA9], cp), "az");
    }

    #[test]
    fn ascii_0037_encodes_known_characters() {
        let cp = codepage("0037");
        let encoded = ascii_to_ebcdic("HELLO", cp);
        let bytes: Vec<u32> = encoded.chars().map(u32::from).collect();
        assert_eq!(bytes, [0xC8, 0xC5, 0xD3, 0xD3, 0xD6]);
    }

    #[test]
    fn printable_ascii_round_trips_for_every_codepage() {
        let printable: String = (0x20u8..0x7F).map(char::from).collect();
        for cp in tables::CODEPAGES.iter() {
            let encoded = ascii_to_ebcdic(&printable, cp);
            let bytes: Vec<u8> = encoded.chars().map(|c| u32::from(c) as u8).collect();
            let decoded = ebcdic_to_ascii(&bytes, cp);
            assert_eq!(
                decoded, printable,
                "codepage {} failed to round-trip",
                cp.id
            );
        }
    }

    #[test]
    fn carriage_return_is_dropped_not_translated() {
        let cp = codepage("0037");
        // CRLF collapses to the single EBCDIC newline byte, and no byte stands in for CR.
        let encoded = ascii_to_ebcdic("a\r\nb", cp);
        let bytes: Vec<u32> = encoded.chars().map(u32::from).collect();
        assert_eq!(
            bytes,
            [0x81, 0x25, 0x82],
            "expected a, NL, b with CR omitted"
        );
        assert_eq!(ascii_to_ebcdic("\r\r\r", cp), "");
    }

    #[test]
    fn characters_above_u00ff_become_nul() {
        let cp = codepage("0037");
        // Upstream reads past its 256-entry table and String.fromCharCode(NaN) gives U+0000.
        assert_eq!(ascii_to_ebcdic("\u{20AC}", cp), "\0");
        assert_eq!(ascii_to_ebcdic("a\u{4E2D}b", cp).chars().count(), 3);
    }

    #[test]
    fn every_byte_decodes_for_every_codepage() {
        let all: Vec<u8> = (0..=255).collect();
        for cp in tables::CODEPAGES.iter() {
            let decoded = ebcdic_to_ascii(&all, cp);
            // Post-patch every entry is exactly one character, so 256 bytes yield 256 chars.
            assert_eq!(
                decoded.chars().count(),
                256,
                "codepage {} dropped or added characters",
                cp.id
            );
        }
    }

    #[test]
    fn upstream_cp0277_carriage_return_defect_is_patched() {
        // Upstream maps 0277 byte 0x0D to "", silently deleting CRs. tools/gen_tables.py
        // corrects it; assert both the correction and that it is the only one.
        assert_eq!(tables::PATCHES.len(), 1);
        assert_eq!(tables::PATCHES[0], ("0277", 0x0D, "", "\r"));
        assert_eq!(ebcdic_to_ascii(&[0x0D], codepage("0277")), "\r");

        // Every codepage now agrees on CR, which is the property that was violated.
        for cp in tables::CODEPAGES.iter() {
            assert_eq!(
                ebcdic_to_ascii(&[0x0D], cp),
                "\r",
                "codepage {} maps CR wrongly",
                cp.id
            );
        }
    }

    #[test]
    fn text_path_maps_characters_not_utf8_bytes() {
        let cp = codepage("0037");
        // U+00C8 is one character but two UTF-8 bytes. Upstream indexes by char code, so this
        // must produce one output character; a bytes-based implementation would produce two.
        let decoded = ebcdic_text_to_ascii("\u{c8}", cp);
        assert_eq!(decoded.chars().count(), 1);
        assert_eq!(decoded, "H", "0xC8 is 'H' in codepage 037");

        // The same content as real bytes must agree with the text path.
        assert_eq!(ebcdic_to_ascii(&[0xC8], cp), decoded);
    }

    #[test]
    fn text_path_passes_through_characters_above_u00ff() {
        let cp = codepage("0037");
        // Upstream would append the literal string "undefined" here; we preserve instead.
        assert_eq!(ebcdic_text_to_ascii("\u{fffd}", cp), "\u{fffd}");
        assert_eq!(ebcdic_text_to_ascii("\u{4e2d}", cp), "\u{4e2d}");
        assert!(!ebcdic_text_to_ascii("\u{4e2d}", cp).contains("undefined"));
    }

    #[test]
    fn convert_dispatches_on_direction_and_source() {
        let cp = codepage("0037");
        let bytes = Source::Bytes(vec![0xC8, 0xC5, 0xD3, 0xD3, 0xD6]);
        assert_eq!(convert(&bytes, Direction::EbcdicToAscii, cp), "HELLO");

        let text = Source::Text("HELLO");
        let encoded = convert(&text, Direction::AsciiToEbcdic, cp);
        let raw: Vec<u32> = encoded.chars().map(u32::from).collect();
        assert_eq!(raw, [0xC8, 0xC5, 0xD3, 0xD3, 0xD6]);
    }

    #[test]
    fn titles_match_upstream_command_titles() {
        assert_eq!(
            Direction::EbcdicToAscii.title("0037"),
            "Convert Ebcdic0037 to Ascii"
        );
        assert_eq!(
            Direction::AsciiToEbcdic.title("1047"),
            "Convert Ascii to Ebcdic1047"
        );
    }

    #[test]
    fn direction_round_trips_through_its_identifier() {
        for direction in [Direction::EbcdicToAscii, Direction::AsciiToEbcdic] {
            assert_eq!(Direction::from_str(direction.as_str()), Some(direction));
        }
        assert_eq!(Direction::from_str("nonsense"), None);
    }
}

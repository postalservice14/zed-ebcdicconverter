//! Cached buffer contents plus the position arithmetic needed to slice and replace them.
//!
//! LSP positions are line/character pairs where `character` counts **UTF-16 code units**, not
//! bytes and not characters. That distinction is easy to ignore and wrong to ignore here: the
//! buffers this server sees are frequently mojibake full of multi-byte characters, so naive
//! byte indexing would slice mid-character and panic or corrupt the edit.

use std::collections::HashMap;
use std::path::PathBuf;

use lsp_types::{Position, Range, Uri};

/// Buffer contents the editor has told us about, keyed by document URI.
#[derive(Debug, Default)]
pub struct Documents {
    texts: HashMap<String, String>,
}

impl Documents {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(&mut self, uri: &Uri, text: String) {
        self.texts.insert(uri.as_str().to_string(), text);
    }

    /// Replace the cached text. The server advertises full sync, so every change carries the
    /// whole document and there is no incremental patching to do.
    pub fn update(&mut self, uri: &Uri, text: String) {
        self.texts.insert(uri.as_str().to_string(), text);
    }

    pub fn close(&mut self, uri: &Uri) {
        self.texts.remove(uri.as_str());
    }

    pub fn get(&self, uri: &Uri) -> Option<&str> {
        self.texts.get(uri.as_str()).map(String::as_str)
    }
}

/// Byte offset in `text` for an LSP position, clamped to the end of the line and of the text.
///
/// Clamping rather than failing is deliberate: a stale position from a racing edit should
/// produce a harmless edit at the boundary, not an error dialog.
pub fn byte_offset(text: &str, position: Position) -> usize {
    let mut offset = 0usize;
    for (index, line) in split_lines(text).enumerate() {
        if index == position.line as usize {
            return offset + utf16_to_byte_offset(line, position.character as usize);
        }
        offset += line.len() + line_terminator_len(text, offset + line.len());
    }
    text.len()
}

/// Byte offset within a single line for a UTF-16 code-unit offset.
fn utf16_to_byte_offset(line: &str, utf16_offset: usize) -> usize {
    let mut units = 0usize;
    for (byte_index, character) in line.char_indices() {
        if units >= utf16_offset {
            return byte_index;
        }
        units += character.len_utf16();
    }
    line.len()
}

/// UTF-16 code-unit length of a line, which is what an LSP `character` field must contain.
fn utf16_len(line: &str) -> usize {
    line.chars().map(char::len_utf16).sum()
}

/// Length of the line terminator starting at `offset`, treating CRLF as one terminator.
fn line_terminator_len(text: &str, offset: usize) -> usize {
    let rest = &text.as_bytes()[offset.min(text.len())..];
    match rest {
        [b'\r', b'\n', ..] => 2,
        [b'\r', ..] | [b'\n', ..] => 1,
        _ => 0,
    }
}

/// Split into lines on LF, CR, or CRLF, keeping a trailing empty line when the text ends with
/// a terminator. Mirrors how an editor counts lines, which is what positions refer to.
fn split_lines(text: &str) -> impl Iterator<Item = &str> {
    let mut lines = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\n' => {
                lines.push(&text[start..index]);
                index += 1;
                start = index;
            }
            b'\r' => {
                lines.push(&text[start..index]);
                index += if bytes.get(index + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
                start = index;
            }
            _ => index += 1,
        }
    }
    lines.push(&text[start..]);
    lines.into_iter()
}

/// The range spanning the whole document, from the very start to the end of the last line.
///
/// Matches upstream, which builds its whole-file range from the first line's start to the
/// last line's end.
pub fn full_range(text: &str) -> Range {
    let lines: Vec<&str> = split_lines(text).collect();
    let last_index = lines.len().saturating_sub(1);
    Range {
        start: Position {
            line: 0,
            character: 0,
        },
        end: Position {
            line: last_index as u32,
            character: utf16_len(lines[last_index]) as u32,
        },
    }
}

/// The text covered by `range`, or `None` if the range is inverted.
pub fn slice(text: &str, range: Range) -> Option<&str> {
    let start = byte_offset(text, range.start);
    let end = byte_offset(text, range.end);
    if start > end {
        return None;
    }
    text.get(start..end)
}

/// Whether a range selects nothing, i.e. the user has a bare cursor and no selection.
///
/// This is the signal that reproduces upstream's rule: no selection means convert everything.
pub fn is_empty(range: Range) -> bool {
    range.start == range.end
}

/// Filesystem path for a `file:` URI, or `None` for any other scheme.
///
/// Hand-rolled because `lsp_types::Uri` is a plain URI type with no `to_file_path`, and pulling
/// in a URL crate for one conversion is not worth it. Returning `None` for non-file URIs is
/// meaningful: untitled buffers have nothing on disk, which is exactly when the caller must
/// fall back to buffer text.
pub fn file_path(uri: &Uri) -> Option<PathBuf> {
    let text = uri.as_str();
    let rest = text.strip_prefix("file://")?;
    // Strip an empty or localhost authority; anything else is a network path we cannot read.
    let path = match rest.find('/') {
        Some(0) => rest,
        Some(index) if matches!(&rest[..index], "localhost") => &rest[index..],
        _ => return None,
    };
    let decoded = percent_decode(path)?;

    // Windows URIs look like file:///C:/dir/file, which needs the leading slash removed.
    let bytes = decoded.as_bytes();
    let is_windows_drive = bytes.len() >= 3
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && (bytes[2] == b':' || bytes[2] == b'|');
    Some(PathBuf::from(if is_windows_drive {
        decoded[1..].replace('|', ":")
    } else {
        decoded
    }))
}

/// Decode `%XX` escapes. Returns `None` if the result is not valid UTF-8 or an escape is
/// malformed, since a path we cannot decode is one we must not guess at.
fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = input.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn range(start: (u32, u32), end: (u32, u32)) -> Range {
        Range {
            start: position(start.0, start.1),
            end: position(end.0, end.1),
        }
    }

    #[test]
    fn byte_offset_walks_lines() {
        let text = "abc\ndef\nghi";
        assert_eq!(byte_offset(text, position(0, 0)), 0);
        assert_eq!(byte_offset(text, position(0, 3)), 3);
        assert_eq!(byte_offset(text, position(1, 0)), 4);
        assert_eq!(byte_offset(text, position(2, 3)), 11);
    }

    #[test]
    fn byte_offset_handles_crlf_as_one_terminator() {
        let text = "abc\r\ndef";
        assert_eq!(
            byte_offset(text, position(1, 0)),
            5,
            "CRLF is two bytes, one terminator"
        );
        assert_eq!(byte_offset(text, position(1, 3)), 8);
    }

    #[test]
    fn byte_offset_counts_utf16_units_not_bytes() {
        // 'é' is 2 UTF-8 bytes but 1 UTF-16 unit.
        let text = "éé!";
        assert_eq!(byte_offset(text, position(0, 1)), 2);
        assert_eq!(byte_offset(text, position(0, 2)), 4);
        assert_eq!(byte_offset(text, position(0, 3)), 5);
    }

    #[test]
    fn byte_offset_counts_surrogate_pairs_as_two_units() {
        // U+1F600 is 4 UTF-8 bytes and 2 UTF-16 units.
        let text = "\u{1f600}x";
        assert_eq!(
            byte_offset(text, position(0, 2)),
            4,
            "one astral char is two UTF-16 units"
        );
        assert_eq!(byte_offset(text, position(0, 3)), 5);
    }

    #[test]
    fn byte_offset_clamps_out_of_range_positions() {
        let text = "abc\ndef";
        assert_eq!(
            byte_offset(text, position(0, 99)),
            3,
            "clamped to end of line"
        );
        assert_eq!(
            byte_offset(text, position(99, 0)),
            text.len(),
            "clamped to end of text"
        );
    }

    #[test]
    fn full_range_covers_everything() {
        assert_eq!(full_range("abc"), range((0, 0), (0, 3)));
        assert_eq!(full_range("abc\ndef"), range((0, 0), (1, 3)));
        // A trailing newline means a final empty line, as editors show it.
        assert_eq!(full_range("abc\n"), range((0, 0), (1, 0)));
        assert_eq!(full_range(""), range((0, 0), (0, 0)));
    }

    #[test]
    fn full_range_end_is_in_utf16_units() {
        assert_eq!(
            full_range("ééé"),
            range((0, 0), (0, 3)),
            "3 chars, 6 bytes, 3 UTF-16 units"
        );
        assert_eq!(full_range("\u{1f600}"), range((0, 0), (0, 2)));
    }

    #[test]
    fn full_range_round_trips_through_slice() {
        for text in ["", "abc", "abc\ndef\n", "ééé\r\nx", "\u{1f600}\nz"] {
            assert_eq!(
                slice(text, full_range(text)),
                Some(text),
                "failed for {text:?}"
            );
        }
    }

    #[test]
    fn slice_extracts_a_selection() {
        let text = "hello\nworld";
        assert_eq!(slice(text, range((0, 0), (0, 5))), Some("hello"));
        assert_eq!(slice(text, range((1, 0), (1, 5))), Some("world"));
        assert_eq!(slice(text, range((0, 3), (1, 2))), Some("lo\nwo"));
    }

    #[test]
    fn slice_rejects_an_inverted_range() {
        assert_eq!(slice("hello", range((0, 4), (0, 1))), None);
    }

    #[test]
    fn empty_range_signals_no_selection() {
        assert!(is_empty(range((3, 7), (3, 7))));
        assert!(!is_empty(range((3, 7), (3, 8))));
    }

    fn uri(text: &str) -> Uri {
        text.parse().expect("valid uri")
    }

    #[test]
    fn file_path_extracts_unix_paths() {
        assert_eq!(
            file_path(&uri("file:///tmp/a.dat")),
            Some(PathBuf::from("/tmp/a.dat"))
        );
        assert_eq!(
            file_path(&uri("file://localhost/tmp/a.dat")),
            Some(PathBuf::from("/tmp/a.dat"))
        );
    }

    #[test]
    fn file_path_decodes_percent_escapes() {
        assert_eq!(
            file_path(&uri("file:///tmp/my%20file.dat")),
            Some(PathBuf::from("/tmp/my file.dat"))
        );
        assert_eq!(
            file_path(&uri("file:///tmp/caf%C3%A9.dat")),
            Some(PathBuf::from("/tmp/café.dat"))
        );
    }

    #[test]
    fn file_path_handles_windows_drive_letters() {
        assert_eq!(
            file_path(&uri("file:///C:/data/a.dat")),
            Some(PathBuf::from("C:/data/a.dat"))
        );
    }

    #[test]
    fn file_path_rejects_non_file_and_remote_uris() {
        // Untitled buffers and remote hosts have nothing readable on disk; the caller relies
        // on None to fall back to buffer text rather than reading the wrong file.
        assert_eq!(file_path(&uri("untitled:Untitled-1")), None);
        assert_eq!(file_path(&uri("file://server/share/a.dat")), None);
        assert_eq!(file_path(&uri("https://example.com/a.dat")), None);
    }

    #[test]
    fn documents_store_tracks_open_update_close() {
        let uri: Uri = "file:///tmp/a.txt".parse().expect("valid uri");
        let mut documents = Documents::new();
        assert_eq!(documents.get(&uri), None);

        documents.open(&uri, "first".to_string());
        assert_eq!(documents.get(&uri), Some("first"));

        documents.update(&uri, "second".to_string());
        assert_eq!(documents.get(&uri), Some("second"));

        documents.close(&uri);
        assert_eq!(documents.get(&uri), None);
    }
}

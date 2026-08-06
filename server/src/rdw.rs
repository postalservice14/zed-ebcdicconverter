//! IBM RDW/SDW record de-framing for variable-length mainframe files.
//!
//! Variable-length datasets (`RECFM=VB` / `VBS`) carry no line terminators. Each record is
//! prefixed with a 4-byte Record Descriptor Word, and that descriptor *is* the record boundary.
//! Decoding such a file byte-for-byte therefore yields one enormous line with four bytes of
//! garbage every record; the framing has to be interpreted, not just decoded.
//!
//! Layout of the descriptor:
//!
//! ```text
//! bytes 0..2  record length, big-endian, INCLUDING these four bytes
//! byte  2     segment flag (see SegmentFlag)
//! byte  3     reserved, always zero
//! ```
//!
//! Spanned records (`VBS`) split one logical record across several physical segments, which
//! must be rejoined. Real extracts commonly mix the two: mostly complete records, with a small
//! fraction split into first/last segment pairs.

use std::io::Read;

/// Size of the descriptor prefixing every physical segment.
const DESCRIPTOR_LEN: usize = 4;

/// Refuse absurd lengths early rather than trying to allocate them. Mainframe logical records
/// are kilobytes at most; anything larger means the chain has desynchronised.
const MAX_RECORD_LEN: usize = 1 << 20;

/// How a physical segment relates to its logical record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentFlag {
    /// A whole logical record.
    Complete,
    /// First segment of a spanned record.
    First,
    /// Last segment of a spanned record.
    Last,
    /// Middle segment of a spanned record.
    Middle,
}

impl SegmentFlag {
    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0x00 => Some(SegmentFlag::Complete),
            0x01 => Some(SegmentFlag::First),
            0x02 => Some(SegmentFlag::Last),
            0x03 => Some(SegmentFlag::Middle),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum Error {
    /// The descriptor at `offset` is not a plausible RDW.
    BadDescriptor {
        offset: u64,
        detail: String,
    },
    /// The file ended in the middle of a record.
    Truncated {
        offset: u64,
        expected: usize,
        got: usize,
    },
    /// A spanned record was left unterminated at end of file.
    UnterminatedSegment {
        offset: u64,
    },
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::BadDescriptor { offset, detail } => write!(
                f,
                "invalid record descriptor at byte {offset}: {detail}. \
                 The file may not be RDW-framed; try again without --rdw."
            ),
            Error::Truncated {
                offset,
                expected,
                got,
            } => write!(
                f,
                "record at byte {offset} claims {expected} bytes but only {got} remain"
            ),
            Error::UnterminatedSegment { offset } => write!(
                f,
                "file ends inside a spanned record starting at byte {offset}"
            ),
            Error::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Error::Io(error)
    }
}

/// Streams logical records out of an RDW-framed reader, rejoining spanned segments.
///
/// Streaming rather than reading the file into memory: these extracts run to tens of megabytes
/// and there is no reason to hold one entirely.
pub struct Records<R: Read> {
    reader: R,
    offset: u64,
    /// Accumulated segments of a spanned record in progress.
    pending: Vec<u8>,
    pending_started_at: u64,
    finished: bool,
}

/// Counts describing what a de-framing pass saw, for a summary line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub logical_records: u64,
    pub physical_segments: u64,
    pub spanned_records: u64,
    pub payload_bytes: u64,
}

impl<R: Read> Records<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            offset: 0,
            pending: Vec::new(),
            pending_started_at: 0,
            finished: false,
        }
    }

    /// Read exactly `len` bytes, distinguishing a clean end of file from a truncated one.
    fn read_exact_or_eof(&mut self, len: usize) -> Result<Option<Vec<u8>>, Error> {
        let mut buffer = vec![0u8; len];
        let mut filled = 0usize;
        while filled < len {
            match self.reader.read(&mut buffer[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        if filled == 0 {
            return Ok(None);
        }
        if filled < len {
            return Err(Error::Truncated {
                offset: self.offset,
                expected: len,
                got: filled,
            });
        }
        Ok(Some(buffer))
    }

    /// Next logical record, or `None` at end of file.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Vec<u8>>, Error> {
        loop {
            if self.finished {
                return Ok(None);
            }
            let descriptor_offset = self.offset;
            let Some(descriptor) = self.read_exact_or_eof(DESCRIPTOR_LEN)? else {
                self.finished = true;
                if !self.pending.is_empty() {
                    return Err(Error::UnterminatedSegment {
                        offset: self.pending_started_at,
                    });
                }
                return Ok(None);
            };
            self.offset += DESCRIPTOR_LEN as u64;

            let total_len = u16::from_be_bytes([descriptor[0], descriptor[1]]) as usize;
            let flag =
                SegmentFlag::from_byte(descriptor[2]).ok_or_else(|| Error::BadDescriptor {
                    offset: descriptor_offset,
                    detail: format!("unknown segment flag {:#04x}", descriptor[2]),
                })?;
            if descriptor[3] != 0 {
                return Err(Error::BadDescriptor {
                    offset: descriptor_offset,
                    detail: format!("reserved byte is {:#04x}, expected 0x00", descriptor[3]),
                });
            }
            if total_len < DESCRIPTOR_LEN {
                return Err(Error::BadDescriptor {
                    offset: descriptor_offset,
                    detail: format!("length {total_len} is smaller than the 4-byte descriptor"),
                });
            }
            if total_len > MAX_RECORD_LEN {
                return Err(Error::BadDescriptor {
                    offset: descriptor_offset,
                    detail: format!("length {total_len} exceeds the {MAX_RECORD_LEN}-byte limit"),
                });
            }

            let payload_len = total_len - DESCRIPTOR_LEN;
            let payload = if payload_len == 0 {
                Vec::new()
            } else {
                self.read_exact_or_eof(payload_len)?
                    .ok_or(Error::Truncated {
                        offset: self.offset,
                        expected: payload_len,
                        got: 0,
                    })?
            };
            self.offset += payload_len as u64;

            match flag {
                SegmentFlag::Complete if self.pending.is_empty() => return Ok(Some(payload)),
                // A complete record arriving mid-span means the chain is inconsistent; surface
                // it rather than silently discarding the partial record.
                SegmentFlag::Complete => {
                    return Err(Error::BadDescriptor {
                        offset: descriptor_offset,
                        detail: "complete record found inside a spanned record".to_string(),
                    })
                }
                SegmentFlag::First => {
                    self.pending = payload;
                    self.pending_started_at = descriptor_offset;
                }
                SegmentFlag::Middle => self.pending.extend_from_slice(&payload),
                SegmentFlag::Last => {
                    self.pending.extend_from_slice(&payload);
                    return Ok(Some(std::mem::take(&mut self.pending)));
                }
            }
        }
    }
}

/// Walk every record, invoking `emit` per logical record, and return counts.
pub fn for_each_record<R: Read, F>(reader: R, mut emit: F) -> Result<Stats, Error>
where
    F: FnMut(&[u8]) -> Result<(), Error>,
{
    let mut records = Records::new(reader);
    let mut stats = Stats::default();
    let mut previous_offset = 0u64;
    while let Some(record) = records.next()? {
        // Segment count is derived from how far the reader advanced: a record spanning two
        // segments consumed two descriptors.
        let consumed = records.offset - previous_offset;
        previous_offset = records.offset;
        let segments = consumed.saturating_sub(record.len() as u64) / DESCRIPTOR_LEN as u64;
        stats.physical_segments += segments.max(1);
        if segments > 1 {
            stats.spanned_records += 1;
        }
        stats.logical_records += 1;
        stats.payload_bytes += record.len() as u64;
        emit(&record)?;
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a physical segment: 2-byte length including descriptor, flag, reserved zero.
    fn segment(flag: u8, payload: &[u8]) -> Vec<u8> {
        let total = (payload.len() + DESCRIPTOR_LEN) as u16;
        let mut out = total.to_be_bytes().to_vec();
        out.push(flag);
        out.push(0x00);
        out.extend_from_slice(payload);
        out
    }

    fn collect(bytes: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let mut records = Records::new(bytes);
        let mut out = Vec::new();
        while let Some(record) = records.next()? {
            out.push(record);
        }
        Ok(out)
    }

    #[test]
    fn reads_complete_records() {
        let mut file = segment(0x00, b"first");
        file.extend(segment(0x00, b"second"));
        assert_eq!(
            collect(&file).unwrap(),
            [b"first".to_vec(), b"second".to_vec()]
        );
    }

    #[test]
    fn rejoins_a_first_last_segment_pair() {
        // The common two-segment shape: a first segment (0x01) then a last segment (0x02).
        let mut file = segment(0x01, b"HELLO ");
        file.extend(segment(0x02, b"WORLD"));
        assert_eq!(collect(&file).unwrap(), [b"HELLO WORLD".to_vec()]);
    }

    #[test]
    fn rejoins_a_span_with_middle_segments() {
        let mut file = segment(0x01, b"AA");
        file.extend(segment(0x03, b"BB"));
        file.extend(segment(0x03, b"CC"));
        file.extend(segment(0x02, b"DD"));
        assert_eq!(collect(&file).unwrap(), [b"AABBCCDD".to_vec()]);
    }

    #[test]
    fn interleaves_complete_and_spanned_records() {
        let mut file = segment(0x00, b"one");
        file.extend(segment(0x01, b"tw"));
        file.extend(segment(0x02, b"o"));
        file.extend(segment(0x00, b"three"));
        assert_eq!(
            collect(&file).unwrap(),
            [b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]
        );
    }

    #[test]
    fn handles_zero_length_payload() {
        let mut file = segment(0x00, b"");
        file.extend(segment(0x00, b"x"));
        assert_eq!(collect(&file).unwrap(), [Vec::new(), b"x".to_vec()]);
    }

    #[test]
    fn empty_input_yields_no_records() {
        assert_eq!(collect(&[]).unwrap(), Vec::<Vec<u8>>::new());
    }

    #[test]
    fn rejects_an_unknown_segment_flag() {
        // 0x04 is not a valid flag; a plain text file hits this almost immediately, which is
        // what makes --rdw safe to pass by mistake.
        let file = segment(0x04, b"x");
        assert!(matches!(
            collect(&file),
            Err(Error::BadDescriptor { offset: 0, .. })
        ));
    }

    #[test]
    fn rejects_a_nonzero_reserved_byte() {
        let file = vec![0x00, 0x06, 0x00, 0x99, b'a', b'b'];
        assert!(matches!(
            collect(&file),
            Err(Error::BadDescriptor { offset: 0, .. })
        ));
    }

    #[test]
    fn rejects_a_length_below_the_descriptor() {
        let file = vec![0x00, 0x03, 0x00, 0x00];
        assert!(matches!(
            collect(&file),
            Err(Error::BadDescriptor { offset: 0, .. })
        ));
    }

    #[test]
    fn reports_truncation_with_the_offset() {
        // Claims 10 bytes total but supplies only 2 of the 6 payload bytes.
        let file = vec![0x00, 0x0A, 0x00, 0x00, b'a', b'b'];
        match collect(&file) {
            Err(Error::Truncated {
                offset,
                expected,
                got,
            }) => {
                assert_eq!((offset, expected, got), (4, 6, 2));
            }
            other => panic!("expected truncation, got {other:?}"),
        }
    }

    #[test]
    fn reports_an_unterminated_spanned_record() {
        let file = segment(0x01, b"start");
        assert!(matches!(
            collect(&file),
            Err(Error::UnterminatedSegment { offset: 0 })
        ));
    }

    #[test]
    fn rejects_a_complete_record_inside_a_span() {
        let mut file = segment(0x01, b"start");
        file.extend(segment(0x00, b"oops"));
        assert!(matches!(collect(&file), Err(Error::BadDescriptor { .. })));
    }

    #[test]
    fn stats_count_segments_and_spans() {
        let mut file = segment(0x00, b"one");
        file.extend(segment(0x01, b"tw"));
        file.extend(segment(0x02, b"o"));
        let mut seen = Vec::new();
        let stats = for_each_record(&file[..], |record| {
            seen.push(record.to_vec());
            Ok(())
        })
        .unwrap();

        assert_eq!(stats.logical_records, 2);
        assert_eq!(
            stats.physical_segments, 3,
            "the spanned record used two segments"
        );
        assert_eq!(stats.spanned_records, 1);
        assert_eq!(stats.payload_bytes, 6, "\"one\" plus \"two\"");
    }
}

//! `ebcdic-lsp convert` — batch file conversion.
//!
//! Code actions cannot reach real mainframe data files. Zed classifies any file with a stray
//! NUL byte in its first 1024 bytes as binary and refuses to open it at all
//! (`analyze_byte_content` in `crates/language/src/file_content.rs`), so there is no buffer, no
//! `didOpen`, and no code action. Variable-length EBCDIC extracts are full of NULs -- from RDW
//! framing and from packed-decimal fields -- so they need a path that never involves a buffer.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::PathBuf;

use crate::convert::{ascii_to_ebcdic, ebcdic_to_ascii, Direction};
use crate::rdw;
use crate::tables::{Codepage, CODEPAGES};

const DEFAULT_CODEPAGE: &str = "0037";

/// Read/write in large blocks: these files run to tens of megabytes.
const BUFFER_SIZE: usize = 1 << 16;

pub const HELP: &str = "\
ebcdic-lsp — convert between ASCII and EBCDIC

USAGE:
    ebcdic-lsp                          Run as a language server on stdin/stdout (default)
    ebcdic-lsp convert [OPTIONS] <FILE> Convert a file

OPTIONS:
    -c, --codepage <ID>   EBCDIC codepage (default: 0037)
    -o, --output <PATH>   Write here instead of stdout
    -r, --rdw             Interpret IBM RDW/SDW framing: strip descriptors, rejoin spanned
                          records, and emit one line per logical record. Required for
                          variable-length (RECFM=VB/VBS) mainframe extracts.
        --to-ebcdic       Encode text to EBCDIC instead of decoding
        --raw             Do not replace control characters (see below)
    -q, --quiet           Suppress the summary written to stderr
    -h, --help            Print this help
    -V, --version         Print version

CONTROL CHARACTERS:
    Decoded output normally has non-printable control characters replaced with '.', because
    packed-decimal and binary fields decode to arbitrary bytes -- including NUL, which would
    make the output itself unopenable in Zed. Pass --raw for byte-faithful output.

CODEPAGES:
    0037 0273 0277 0278 0280 0284 0285 0297 0500 0871 1047

EXAMPLES:
    ebcdic-lsp convert --rdw -c 0037 extract.dat -o extract.txt
    ebcdic-lsp convert flat.dat | head -50
";

#[derive(Debug, PartialEq, Eq)]
pub struct Options {
    pub input: PathBuf,
    pub output: Option<PathBuf>,
    pub codepage: String,
    pub direction: Direction,
    pub rdw: bool,
    pub raw: bool,
    pub quiet: bool,
}

/// What the user asked for on the command line.
#[derive(Debug, PartialEq, Eq)]
pub enum Invocation {
    /// No arguments: behave as a language server, which is how Zed launches us.
    LanguageServer,
    Convert(Options),
    Help,
    Version,
}

pub fn parse(arguments: &[String]) -> Result<Invocation, String> {
    let mut arguments = arguments.iter().map(String::as_str);
    let Some(first) = arguments.next() else {
        return Ok(Invocation::LanguageServer);
    };

    match first {
        "-h" | "--help" | "help" => return Ok(Invocation::Help),
        "-V" | "--version" => return Ok(Invocation::Version),
        "convert" => {}
        other => {
            return Err(format!(
                "unknown argument {other:?}. Run with --help for usage."
            ))
        }
    }

    let mut input: Option<PathBuf> = None;
    let mut output = None;
    let mut codepage = DEFAULT_CODEPAGE.to_string();
    let mut direction = Direction::EbcdicToAscii;
    let (mut rdw, mut raw, mut quiet) = (false, false, false);

    while let Some(argument) = arguments.next() {
        match argument {
            "-c" | "--codepage" => {
                codepage = arguments
                    .next()
                    .ok_or("--codepage needs a value, e.g. --codepage 0037")?
                    .to_string();
            }
            "-o" | "--output" => {
                output = Some(PathBuf::from(
                    arguments.next().ok_or("--output needs a path")?,
                ));
            }
            "-r" | "--rdw" => rdw = true,
            "--to-ebcdic" => direction = Direction::AsciiToEbcdic,
            "--raw" => raw = true,
            "-q" | "--quiet" => quiet = true,
            "-h" | "--help" => return Ok(Invocation::Help),
            other if other.starts_with('-') && other != "-" => {
                return Err(format!(
                    "unknown option {other:?}. Run with --help for usage."
                ));
            }
            path => {
                if input.replace(PathBuf::from(path)).is_some() {
                    return Err("only one input file may be given".to_string());
                }
            }
        }
    }

    let input = input.ok_or("convert needs an input file. Run with --help for usage.")?;

    // Re-framing on encode would mean inventing record boundaries the input does not carry.
    if rdw && direction == Direction::AsciiToEbcdic {
        return Err(
            "--rdw applies to decoding only; it cannot be combined with --to-ebcdic".to_string(),
        );
    }

    Ok(Invocation::Convert(Options {
        input,
        output,
        codepage,
        direction,
        rdw,
        raw,
        quiet,
    }))
}

fn lookup(id: &str) -> Result<&'static Codepage, String> {
    let normalized = normalize(id);
    CODEPAGES
        .iter()
        .find(|codepage| codepage.id == normalized)
        .ok_or_else(|| {
            let known: Vec<&str> = CODEPAGES.iter().map(|c| c.id).collect();
            format!(
                "unknown codepage {id:?}. Known codepages: {}.",
                known.join(", ")
            )
        })
}

/// Accept `37`, `037`, `0037`, `cp037`, `ibm-037`.
fn normalize(id: &str) -> String {
    let lowered = id.trim().to_ascii_lowercase();
    let digits = lowered
        .trim_start_matches("cp")
        .trim_start_matches("ibm-")
        .trim_start_matches("ibm");
    format!("{:0>4}", digits.trim_start_matches('0'))
}

/// Replace characters that would make the output unreadable, or unopenable.
///
/// `preserve_newlines` is false when the caller supplies its own record separators: a decoded
/// EBCDIC NL inside a record payload would otherwise split one record across two lines and
/// break the one-line-per-record guarantee.
pub fn sanitize(text: &str, preserve_newlines: bool) -> String {
    text.chars()
        .map(|character| match character {
            '\n' | '\r' if preserve_newlines => character,
            '\t' => character,
            // C0 controls, DEL, and C1 controls. EBCDIC data fields decode into all of these.
            character if (character as u32) < 0x20 => '.',
            character if ('\u{7f}'..='\u{9f}').contains(&character) => '.',
            character => character,
        })
        .collect()
}

pub fn run(options: &Options) -> Result<(), String> {
    let codepage = lookup(&options.codepage)?;

    let input = File::open(&options.input)
        .map_err(|error| format!("cannot open {}: {error}", options.input.display()))?;
    let reader = BufReader::with_capacity(BUFFER_SIZE, input);

    let mut writer: Box<dyn Write> = match &options.output {
        Some(path) => Box::new(BufWriter::with_capacity(
            BUFFER_SIZE,
            File::create(path)
                .map_err(|error| format!("cannot create {}: {error}", path.display()))?,
        )),
        None => Box::new(BufWriter::with_capacity(BUFFER_SIZE, io::stdout())),
    };

    let summary = if options.rdw {
        convert_framed(reader, &mut writer, codepage, options)?
    } else {
        convert_flat(reader, &mut writer, codepage, options)?
    };

    writer
        .flush()
        .map_err(|error| format!("failed writing output: {error}"))?;

    if !options.quiet {
        eprintln!("{summary}");
    }
    Ok(())
}

/// De-frame and decode, emitting one line per logical record.
fn convert_framed(
    reader: impl Read,
    writer: &mut dyn Write,
    codepage: &Codepage,
    options: &Options,
) -> Result<String, String> {
    let mut write_error = None;
    let stats = rdw::for_each_record(reader, |record| {
        let decoded = ebcdic_to_ascii(record, codepage);
        let line = if options.raw {
            decoded
        } else {
            sanitize(&decoded, false)
        };
        if let Err(error) = writeln!(writer, "{line}") {
            write_error = Some(error.to_string());
            // Reported through `write_error`; this just stops the walk.
            return Err(rdw::Error::Io(io::Error::other("output closed")));
        }
        Ok(())
    });

    if let Some(error) = write_error {
        return Err(format!("failed writing output: {error}"));
    }
    let stats = stats.map_err(|error| error.to_string())?;

    Ok(format!(
        "{} logical records ({} physical segments, {} spanned) from {} payload bytes, codepage {}",
        stats.logical_records,
        stats.physical_segments,
        stats.spanned_records,
        stats.payload_bytes,
        codepage.id
    ))
}

/// Convert a flat file with no record framing.
fn convert_flat(
    mut reader: impl Read,
    writer: &mut dyn Write,
    codepage: &Codepage,
    options: &Options,
) -> Result<String, String> {
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(|error| format!("failed reading input: {error}"))?;

    let converted = match options.direction {
        Direction::EbcdicToAscii => {
            let decoded = ebcdic_to_ascii(&bytes, codepage);
            if options.raw {
                decoded
            } else {
                // Newlines are real content here: EBCDIC NL is what separates the lines.
                sanitize(&decoded, true)
            }
        }
        Direction::AsciiToEbcdic => {
            let text = String::from_utf8_lossy(&bytes);
            ascii_to_ebcdic(&text, codepage)
        }
    };

    // Encoded output is characters in U+0000..U+00FF whose scalars are the EBCDIC bytes, so it
    // must be written as those bytes, not as UTF-8.
    match options.direction {
        Direction::AsciiToEbcdic => {
            let raw: Vec<u8> = converted.chars().map(|c| u32::from(c) as u8).collect();
            writer
                .write_all(&raw)
                .map_err(|error| format!("failed writing output: {error}"))?;
        }
        Direction::EbcdicToAscii => {
            writer
                .write_all(converted.as_bytes())
                .map_err(|error| format!("failed writing output: {error}"))?;
        }
    }

    let verb = match options.direction {
        Direction::EbcdicToAscii => "decoded",
        Direction::AsciiToEbcdic => "encoded",
    };
    Ok(format!(
        "{verb} {} bytes, codepage {}",
        bytes.len(),
        codepage.id
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(arguments: &[&str]) -> Result<Invocation, String> {
        let owned: Vec<String> = arguments.iter().map(|s| s.to_string()).collect();
        parse(&owned)
    }

    fn options_of(invocation: Invocation) -> Options {
        match invocation {
            Invocation::Convert(options) => options,
            other => panic!("expected a convert invocation, got {other:?}"),
        }
    }

    #[test]
    fn no_arguments_means_language_server() {
        // Zed launches the binary with no arguments, so this must stay the default.
        assert_eq!(parse_args(&[]).unwrap(), Invocation::LanguageServer);
    }

    #[test]
    fn parses_a_minimal_convert_invocation() {
        let options = options_of(parse_args(&["convert", "a.dat"]).unwrap());
        assert_eq!(options.input, PathBuf::from("a.dat"));
        assert_eq!(options.codepage, "0037", "defaults to codepage 0037");
        assert_eq!(options.direction, Direction::EbcdicToAscii);
        assert!(!options.rdw && !options.raw && !options.quiet);
        assert_eq!(options.output, None);
    }

    #[test]
    fn parses_all_options_in_any_order() {
        let options = options_of(
            parse_args(&[
                "convert", "-r", "-c", "1047", "--quiet", "in.dat", "-o", "out.txt",
            ])
            .unwrap(),
        );
        assert_eq!(options.codepage, "1047");
        assert_eq!(options.input, PathBuf::from("in.dat"));
        assert_eq!(options.output, Some(PathBuf::from("out.txt")));
        assert!(options.rdw && options.quiet);
    }

    #[test]
    fn help_and_version_short_circuit() {
        for argument in ["-h", "--help", "help"] {
            assert_eq!(parse_args(&[argument]).unwrap(), Invocation::Help);
        }
        assert_eq!(parse_args(&["-V"]).unwrap(), Invocation::Version);
    }

    #[test]
    fn rejects_rdw_with_to_ebcdic() {
        // Re-framing would require inventing record boundaries the input does not have.
        let error = parse_args(&["convert", "--rdw", "--to-ebcdic", "a.txt"]).unwrap_err();
        assert!(error.contains("decoding only"), "got {error:?}");
    }

    #[test]
    fn rejects_missing_input_unknown_flags_and_double_input() {
        assert!(parse_args(&["convert"]).unwrap_err().contains("input file"));
        assert!(parse_args(&["convert", "--nope", "a"])
            .unwrap_err()
            .contains("unknown option"));
        assert!(parse_args(&["convert", "a", "b"])
            .unwrap_err()
            .contains("only one input"));
        assert!(parse_args(&["--nope"])
            .unwrap_err()
            .contains("unknown argument"));
        assert!(parse_args(&["convert", "-c"])
            .unwrap_err()
            .contains("--codepage"));
    }

    #[test]
    fn codepage_lookup_accepts_spellings_and_rejects_unknowns() {
        for spelling in ["0037", "037", "37", "cp037", "IBM-037"] {
            assert_eq!(lookup(spelling).unwrap().id, "0037");
        }
        let error = lookup("9999").unwrap_err();
        assert!(
            error.contains("Known codepages"),
            "error should list valid ids: {error}"
        );
    }

    #[test]
    fn sanitize_replaces_nul_and_control_characters() {
        // The point of this: a NUL in the output would make Zed refuse to open it, defeating
        // the entire purpose of converting the file.
        assert_eq!(sanitize("a\0b", true), "a.b");
        assert_eq!(sanitize("a\u{1}\u{1f}\u{7f}\u{9f}b", true), "a....b");
        assert!(!sanitize("\0\0\0", true).contains('\0'));
    }

    #[test]
    fn sanitize_preserves_newlines_only_when_asked() {
        assert_eq!(sanitize("a\nb\tc", true), "a\nb\tc");
        // Record mode supplies its own separators, so an embedded NL must not split the record.
        assert_eq!(sanitize("a\nb\tc", false), "a.b\tc");
    }

    #[test]
    fn sanitize_leaves_printable_and_accented_text_alone() {
        assert_eq!(
            sanitize("Hello, World! 123 £é", true),
            "Hello, World! 123 £é"
        );
    }
}

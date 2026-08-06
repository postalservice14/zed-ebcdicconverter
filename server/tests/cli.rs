//! End-to-end tests for `ebcdic-lsp convert`, driving the real binary.
//!
//! All fixtures are synthetic and built in-process. Real extracts may contain sensitive data and
//! do not belong in this repository.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Build a physical RDW segment: 2-byte length including the descriptor, flag, reserved zero.
fn segment(flag: u8, payload: &[u8]) -> Vec<u8> {
    let total = (payload.len() + 4) as u16;
    let mut out = total.to_be_bytes().to_vec();
    out.push(flag);
    out.push(0x00);
    out.extend_from_slice(payload);
    out
}

/// Encode ASCII to EBCDIC codepage 037 for the handful of characters the tests use.
fn to_cp037(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| match c {
            'A'..='I' => 0xC1 + (c as u8 - b'A'),
            'J'..='R' => 0xD1 + (c as u8 - b'J'),
            'S'..='Z' => 0xE2 + (c as u8 - b'S'),
            '0'..='9' => 0xF0 + (c as u8 - b'0'),
            ' ' => 0x40,
            other => panic!("test helper does not encode {other:?}"),
        })
        .collect()
}

struct Fixture {
    directory: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let directory = std::env::temp_dir().join(format!("ebcdic-cli-{name}"));
        std::fs::remove_dir_all(&directory).ok();
        std::fs::create_dir_all(&directory).expect("create fixture dir");
        Self { directory }
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.directory.join(name);
        std::fs::write(&path, bytes).expect("write fixture");
        path
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.join(name)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.directory).ok();
    }
}

fn run(arguments: &[&dyn AsRef<Path>]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ebcdic-lsp"));
    for argument in arguments {
        command.arg(argument.as_ref());
    }
    command.output().expect("run ebcdic-lsp")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn decodes_an_rdw_framed_file_one_line_per_record() {
    let fixture = Fixture::new("rdw-basic");
    let mut bytes = segment(0x00, &to_cp037("HELLO"));
    bytes.extend(segment(0x00, &to_cp037("WORLD")));
    let input = fixture.write("a.dat", &bytes);

    let output = run(&[&"convert", &"--rdw", &input]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(stdout_of(&output), "HELLO\nWORLD\n");
    assert!(
        stderr_of(&output).contains("2 logical records"),
        "{}",
        stderr_of(&output)
    );
}

#[test]
fn rejoins_spanned_records_into_one_line() {
    // The common two-segment shape: a first segment (0x01) then a last segment (0x02).
    let fixture = Fixture::new("rdw-spanned");
    let mut bytes = segment(0x01, &to_cp037("HELLO "));
    bytes.extend(segment(0x02, &to_cp037("WORLD")));
    let input = fixture.write("a.dat", &bytes);

    let output = run(&[&"convert", &"--rdw", &input]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(
        stdout_of(&output),
        "HELLO WORLD\n",
        "one logical record, one line"
    );

    let summary = stderr_of(&output);
    assert!(summary.contains("1 logical records"), "{summary}");
    assert!(summary.contains("2 physical segments"), "{summary}");
    assert!(summary.contains("1 spanned"), "{summary}");
}

#[test]
fn replaces_control_characters_so_the_output_opens_in_zed() {
    // This is the property the whole CLI exists for: packed-decimal fields decode to arbitrary
    // bytes including NUL, and a NUL anywhere in the first 1024 bytes makes Zed refuse the file.
    let fixture = Fixture::new("rdw-controls");
    let mut payload = to_cp037("AB");
    payload.extend([0x00, 0x01, 0x02, 0x25]); // NUL, SOH, STX, and EBCDIC newline
    payload.extend(to_cp037("CD"));
    let input = fixture.write("a.dat", &segment(0x00, &payload));

    let output = run(&[&"convert", &"--rdw", &input]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(stdout_of(&output), "AB....CD\n");
    assert!(
        !output.stdout.contains(&0x00),
        "output must contain no NUL bytes"
    );
    assert_eq!(
        stdout_of(&output).lines().count(),
        1,
        "an embedded EBCDIC newline must not split the record"
    );
}

#[test]
fn raw_mode_preserves_control_characters() {
    let fixture = Fixture::new("rdw-raw");
    let mut payload = to_cp037("AB");
    payload.push(0x00);
    let input = fixture.write("a.dat", &segment(0x00, &payload));

    let output = run(&[&"convert", &"--rdw", &"--raw", &input]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    assert!(output.stdout.contains(&0x00), "--raw must be byte-faithful");
}

#[test]
fn writes_to_an_output_file_and_stays_quiet() {
    let fixture = Fixture::new("rdw-output");
    let input = fixture.write("a.dat", &segment(0x00, &to_cp037("HELLO")));
    let destination = fixture.path("out.txt");

    let output = run(&[
        &"convert",
        &"--rdw",
        &input,
        &"-o",
        &destination,
        &"--quiet",
    ]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(std::fs::read_to_string(&destination).unwrap(), "HELLO\n");
    assert!(
        stdout_of(&output).is_empty(),
        "output went to the file, not stdout"
    );
    assert!(
        stderr_of(&output).is_empty(),
        "--quiet suppresses the summary"
    );
}

#[test]
fn decodes_a_flat_file_preserving_its_newlines() {
    let fixture = Fixture::new("flat");
    let mut bytes = to_cp037("LINE1");
    bytes.push(0x25); // EBCDIC newline: real content in a flat file
    bytes.extend(to_cp037("LINE2"));
    let input = fixture.write("a.dat", &bytes);

    let output = run(&[&"convert", &input]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(stdout_of(&output), "LINE1\nLINE2");
}

#[test]
fn encodes_text_to_ebcdic_bytes() {
    let fixture = Fixture::new("encode");
    let input = fixture.write("a.txt", b"HELLO");
    let destination = fixture.path("out.ebc");

    let output = run(&[&"convert", &"--to-ebcdic", &input, &"-o", &destination]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(std::fs::read(&destination).unwrap(), to_cp037("HELLO"));
}

#[test]
fn round_trips_a_flat_file_through_both_directions() {
    let fixture = Fixture::new("roundtrip");
    let original = "THE QUICK BROWN FOX 1234";
    let text = fixture.write("a.txt", original.as_bytes());
    let encoded = fixture.path("b.ebc");
    let decoded = fixture.path("c.txt");

    assert!(run(&[&"convert", &"--to-ebcdic", &text, &"-o", &encoded])
        .status
        .success());
    assert!(run(&[&"convert", &encoded, &"-o", &decoded])
        .status
        .success());
    assert_eq!(std::fs::read_to_string(&decoded).unwrap(), original);
}

#[test]
fn selects_a_codepage_and_reports_unknown_ones() {
    let fixture = Fixture::new("codepage");
    let input = fixture.write("a.dat", &segment(0x00, &to_cp037("HELLO")));

    // Codepage 1047 shares letter mappings with 037, so this must still decode.
    let output = run(&[&"convert", &"--rdw", &"-c", &"1047", &input]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(stdout_of(&output), "HELLO\n");

    let bad = run(&[&"convert", &"--rdw", &"-c", &"9999", &input]);
    assert!(!bad.status.success());
    assert!(
        stderr_of(&bad).contains("Known codepages"),
        "{}",
        stderr_of(&bad)
    );
}

#[test]
fn rdw_on_a_non_framed_file_fails_with_a_useful_hint() {
    // Passing --rdw by mistake must not emit garbage; it should say what to do instead.
    let fixture = Fixture::new("not-framed");
    let input = fixture.write(
        "a.txt",
        b"just some plain ascii text, definitely not framed",
    );

    let output = run(&[&"convert", &"--rdw", &input]);
    assert!(!output.status.success());
    let error = stderr_of(&output);
    assert!(
        error.contains("without --rdw"),
        "should suggest the fix: {error}"
    );
}

#[test]
fn reports_a_missing_input_file() {
    let output = run(&[&"convert", &"/nonexistent/nope.dat"]);
    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("cannot open"),
        "{}",
        stderr_of(&output)
    );
}

#[test]
fn rejects_rdw_combined_with_encoding() {
    let output = run(&[&"convert", &"--rdw", &"--to-ebcdic", &"a.txt"]);
    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("decoding only"),
        "{}",
        stderr_of(&output)
    );
}

#[test]
fn help_and_version_succeed() {
    let help = run(&[&"--help"]);
    assert!(help.status.success());
    assert!(stdout_of(&help).contains("USAGE"));
    assert!(stdout_of(&help).contains("--rdw"));

    let version = run(&[&"--version"]);
    assert!(version.status.success());
    assert!(stdout_of(&version).starts_with("ebcdic-lsp "));
}

#[test]
fn unknown_arguments_fail_rather_than_starting_a_server() {
    // A typo must not leave the process silently waiting on stdin for LSP traffic.
    let output = run(&[&"--nonsense"]);
    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("unknown argument"),
        "{}",
        stderr_of(&output)
    );
}

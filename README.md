# EBCDIC Converter for Zed

Convert text between ASCII and 11 EBCDIC codepages inside [Zed](https://zed.dev). A port of
[CoderAllan/vscode-ebcdicconverter](https://github.com/CoderAllan/vscode-ebcdicconverter).

Codepages: `0037` `0273` `0277` `0278` `0280` `0284` `0285` `0297` `0500` `0871` `1047`

Two ways in, because one is not enough:

- **Code actions** for text you can see in the editor — selections, snippets, small files.
- **A CLI** (`ebcdic-lsp convert`) for real mainframe data files, which Zed refuses to open at
  all. See [Data files](#data-files-the-cli).

## Usage: code actions

Select text and press `cmd-.` (macOS) or `ctrl-.` (Linux/Windows), then pick a conversion:

```
Convert Ebcdic0037 to Ascii
Convert Ascii to Ebcdic0037
...
```

**With text selected**, only the selection is converted. **With no selection**, the entire file
is converted — same rule as the VS Code extension.

Actions appear on `Plain Text` and `Unknown` buffers. `Unknown` is where most mainframe extracts
land, since Zed cannot classify their extensions.

> **Why the code actions menu and not the command palette?** Zed's extension API cannot do
> anything else. Extensions are WebAssembly components with no access to editor buffers, and the
> manifest has no field for commands or keybindings. LSP code actions are the only mechanism in
> Zed that can rewrite buffer text, so the conversions ship as a tiny language server.

## Data files: the CLI

**Code actions cannot work on real mainframe extracts, and no extension can change that.** Zed
inspects the first 1024 bytes of every file; a NUL byte that does not fit a UTF-16 pattern makes
it `Binary`, and Zed then refuses to open the file (`analyze_byte_content` in
`crates/language/src/file_content.rs`, enforced by `anyhow::ensure!` in `worktree.rs`). No
buffer means no `didOpen`, which means no language server and no code actions. Variable-length
EBCDIC files are full of NULs — from record framing and from packed-decimal fields — so they
need a route that never involves a buffer:

```sh
ebcdic-lsp convert --rdw -c 0037 extract.dat -o extract.txt
# 371939 logical records (373035 physical segments, 1096 spanned) from 31700957 payload bytes, codepage 0037
```

`.zed/tasks.json` wraps this for `task: spawn`. Note that the "current file" tasks rely on
`$ZED_FILE`, which comes from the active editor — so for files Zed won't open, use the
selection-based task or Zed's terminal.

### Why `--rdw` matters

Variable-length datasets (`RECFM=VB`/`VBS`) have **no line terminators**. Every record is
prefixed with a 4-byte Record Descriptor Word, and that descriptor *is* the boundary. Decode
such a file byte-for-byte and you get one enormous line with four bytes of garbage per record.
`--rdw` strips the descriptors, rejoins spanned records from their segments, and emits one line
per logical record.

### Control characters

Decoded output has non-printable control characters replaced with `.` by default. This is not
cosmetic: packed-decimal and binary fields decode to arbitrary bytes including NUL, and a NUL in
the output would make the *converted* file unopenable in Zed too. `--raw` disables it.

In record mode this also applies to newlines inside a record payload, so that one logical record
is always exactly one line. In flat-file mode newlines are preserved, because there the EBCDIC
newline is what separates the lines.

### Working with real files

Keep real extracts outside this repository. All test fixtures are synthetic and built in-process,
so nothing sensitive is ever needed to run the suite.

## Requirements

Zed 1.14 or newer. The extension targets `zed_extension_api` 0.7, which is the highest version
Zed's stable channel accepts as of 1.14.2 (`since_v0_6_0::MAX_VERSION` in `extension_host`);
0.8 is dev/nightly only, so 0.7 is the newest version that works on every channel.

## Install

Not yet in the Zed extension registry. To install as a dev extension:

```sh
git clone https://github.com/postalservice14/zed-ebcdicconverter
cd zed-ebcdicconverter/server && cargo build --release
```

Put the server on your `PATH`, or point Zed at it directly (see below). Then run
`zed: install dev extension` and choose the repository root.

Once released, the extension downloads the right prebuilt server binary from GitHub Releases
automatically and no manual step is needed.

## Configuration

Optional. By default all 11 codepages are offered, which means 22 entries in the code actions
menu. Narrow it in `settings.json`:

```jsonc
{
  "lsp": {
    "ebcdic-lsp": {
      "settings": {
        "codepages": ["0037", "1047"]
      }
    }
  }
}
```

Ids are forgiving: `0037`, `037`, `37`, `cp037`, and `ibm-037` all work. Unrecognised ids are
ignored with a warning in the language server log; if *every* id is unrecognised, all codepages
are offered rather than none.

To use a specific server binary:

```jsonc
{
  "lsp": { "ebcdic-lsp": { "binary": { "path": "/path/to/ebcdic-lsp" } } }
}
```

## How it works

| Piece | Role |
| --- | --- |
| `src/lib.rs` | WASM extension. Locates or downloads the server, forwards settings. |
| `server/src/lsp.rs` | Code actions and `codeAction/resolve`. |
| `server/src/cli.rs` | `convert` subcommand for data files. |
| `server/src/rdw.rs` | IBM RDW/SDW de-framing, including spanned records. |
| `server/src/convert.rs` | The conversion functions themselves. |
| `tools/gen_tables.py` | Derives `server/src/tables.rs` from upstream's TypeScript. |

The binary runs as a language server when given no arguments (which is how Zed launches it) and
as a converter when given `convert`.

Code actions are advertised **without** edits and filled in on `codeAction/resolve`. Zed
refreshes code actions as the cursor moves, so computing all 22 conversions eagerly would
re-convert the whole file on every cursor movement.

### Reading real EBCDIC files

A genuine EBCDIC file is not valid UTF-8, so Zed has already replaced its invalid bytes with
`U+FFFD` by the time anything else sees the buffer — the original bytes are unrecoverable from
the text. So for a **whole-file** EBCDIC→ASCII conversion the server re-reads the raw bytes from
disk. Selections use buffer text, because a selection has no recoverable byte range.

This means EBCDIC files look like mojibake until converted. That is expected.

## Development

```sh
cd server
cargo test           # 77 unit + 14 CLI + 7 protocol tests
cargo clippy --all-targets -- -D warnings
cargo fmt

cd ..
cargo build --release --target wasm32-wasip2   # the extension itself
```

Rust is pinned in `rust-toolchain.toml` (including the `wasm32-wasip2` target). Python for the
table generator is pinned in `.tool-versions`.

### Regenerating the conversion tables

`server/src/tables.rs` is generated, not hand-written — 22 tables × 256 entries is 5,632 values,
where one wrong digit silently corrupts data.

```sh
python3 tools/gen_tables.py           # regenerate from upstream
python3 tools/gen_tables.py --check   # verify the committed file matches upstream (CI does this)
```

The tables were validated against Python's own EBCDIC codecs: for cp037 and cp500 every one of
the 256 byte mappings agrees except `0x15`, where upstream deliberately maps EBCDIC NEL to `\n`
so mainframe line breaks become Unix newlines.

## Differences from the VS Code extension

See [NOTICE](NOTICE) for the full list with rationale. In short:

1. Invocation is the code actions menu, not the command palette (Zed API limitation).
2. Codepage 0277 byte `0x0D` maps to `\r` here. Upstream maps it to the empty string — the only
   codepage that does — silently deleting every carriage return.
3. Selections convert buffer text, avoiding an upstream bug where on-disk selections slice file
   bytes using column offsets and so convert the wrong bytes on any multi-line selection.
4. A `codepages` setting exists; upstream has no configuration.

## License

MIT. See [LICENSE](LICENSE), and [NOTICE](NOTICE) for upstream attribution.

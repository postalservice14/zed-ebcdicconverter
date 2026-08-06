//! `ebcdic-lsp`: converts between ASCII and EBCDIC, two ways in.
//!
//! **As a language server (no arguments).** Zed extensions cannot read or write editor buffers
//! -- the extension API covers language servers, debug adapters, MCP servers and slash commands,
//! none of which can produce an edit. LSP code actions are the one mechanism that can, so the
//! conversions the upstream VS Code extension exposes as palette commands are code actions here.
//!
//! **As a CLI (`convert`).** Code actions require an open buffer, and Zed refuses to open any
//! file with a stray NUL in its first 1024 bytes. Real variable-length EBCDIC extracts are full
//! of NULs, so they can only be converted out-of-editor. See `cli::HELP`.

mod cli;
mod config;
mod convert;
mod document;
mod lsp;
mod rdw;
mod tables;

use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    let invocation = match cli::parse(&arguments) {
        Ok(invocation) => invocation,
        Err(message) => {
            eprintln!("error: {message}");
            return ExitCode::FAILURE;
        }
    };

    match invocation {
        cli::Invocation::LanguageServer => match lsp::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("error: {error}");
                ExitCode::FAILURE
            }
        },
        cli::Invocation::Convert(options) => match cli::run(&options) {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => {
                eprintln!("error: {message}");
                ExitCode::FAILURE
            }
        },
        cli::Invocation::Help => {
            print!("{}", cli::HELP);
            ExitCode::SUCCESS
        }
        cli::Invocation::Version => {
            println!("ebcdic-lsp {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
    }
}

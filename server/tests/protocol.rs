//! End-to-end protocol test: spawns the real binary and speaks LSP over stdio.
//!
//! The unit tests call `Server` methods directly, which skips the initialize handshake, the
//! JSON-RPC framing, and capability serialisation -- exactly the layers where a server that
//! "works" still fails to attach in an editor.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

/// How long to wait for the server to exit after `exit` before declaring it wedged.
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);

struct Server {
    process: Child,
    /// `Option` so the pipe can be closed explicitly: the server's reader thread runs until
    /// stdin reaches EOF, so leaving it open keeps the process alive forever.
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Drop for Server {
    /// Never leak a child process, even when a test panics mid-conversation.
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

impl Server {
    fn start(initialization_options: serde_json::Value) -> Self {
        let mut process = Command::new(env!("CARGO_BIN_EXE_ebcdic-lsp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ebcdic-lsp");
        let stdin = Some(process.stdin.take().expect("stdin"));
        let stdout = BufReader::new(process.stdout.take().expect("stdout"));
        let mut server = Self {
            process,
            stdin,
            stdout,
        };

        server.request(
            1,
            "initialize",
            serde_json::json!({
                "processId": null,
                "rootUri": null,
                "capabilities": {},
                "initializationOptions": initialization_options,
            }),
        );
        let initialized = server.read_message();
        assert!(
            initialized["result"]["capabilities"]["codeActionProvider"].is_object(),
            "server must advertise code action support: {initialized}"
        );
        server.notify("initialized", serde_json::json!({}));
        server
    }

    fn send(&mut self, payload: &serde_json::Value) {
        let body = serde_json::to_string(payload).expect("serialize");
        let stdin = self.stdin.as_mut().expect("stdin still open");
        write!(stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body).expect("write");
        stdin.flush().expect("flush");
    }

    fn request(&mut self, id: i64, method: &str, params: serde_json::Value) {
        self.send(&serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        }));
    }

    fn notify(&mut self, method: &str, params: serde_json::Value) {
        self.send(&serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Read one message, skipping server-initiated notifications such as log messages.
    fn read_response(&mut self) -> serde_json::Value {
        loop {
            let message = self.read_message();
            if message.get("id").is_some() {
                return message;
            }
        }
    }

    fn read_message(&mut self) -> serde_json::Value {
        let mut length = None;
        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).expect("read header");
            assert!(
                read > 0,
                "server closed the connection while reading headers"
            );
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                break;
            }
            if let Some(value) = trimmed.strip_prefix("Content-Length: ") {
                length = Some(value.parse::<usize>().expect("numeric content length"));
            }
        }
        let mut body = vec![0u8; length.expect("Content-Length header")];
        self.stdout.read_exact(&mut body).expect("read body");
        serde_json::from_slice(&body).expect("valid json body")
    }

    fn open(&mut self, uri: &str, text: &str) {
        self.notify(
            "textDocument/didOpen",
            serde_json::json!({ "textDocument": {
                "uri": uri, "languageId": "plaintext", "version": 1, "text": text
            }}),
        );
    }

    fn code_actions(
        &mut self,
        id: i64,
        uri: &str,
        range: serde_json::Value,
    ) -> Vec<serde_json::Value> {
        self.request(
            id,
            "textDocument/codeAction",
            serde_json::json!({
                "textDocument": { "uri": uri },
                "range": range,
                "context": { "diagnostics": [] }
            }),
        );
        self.read_response()["result"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    fn resolve(&mut self, id: i64, action: &serde_json::Value) -> serde_json::Value {
        self.request(id, "codeAction/resolve", action.clone());
        self.read_response()["result"].clone()
    }

    fn shutdown(mut self) {
        self.request(999, "shutdown", serde_json::Value::Null);
        let _ = self.read_response();
        self.notify("exit", serde_json::Value::Null);

        // Close the pipe: the server's stdin reader thread runs to EOF, and `io_threads.join()`
        // will not return while the pipe is still open, so without this the process never exits.
        self.stdin.take();

        // Bounded so a wedged server fails the test instead of hanging the suite.
        let deadline = Instant::now() + EXIT_TIMEOUT;
        loop {
            match self.process.try_wait().expect("poll child") {
                Some(status) => {
                    assert!(
                        status.success() || status.code().is_none(),
                        "expected a clean exit, got {status:?}"
                    );
                    return;
                }
                None if Instant::now() >= deadline => {
                    panic!("server did not exit within {EXIT_TIMEOUT:?} of the exit notification");
                }
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }
}

fn position(line: u32, character: u32) -> serde_json::Value {
    serde_json::json!({ "line": line, "character": character })
}

fn range(start: (u32, u32), end: (u32, u32)) -> serde_json::Value {
    serde_json::json!({ "start": position(start.0, start.1), "end": position(end.0, end.1) })
}

fn find(actions: &[serde_json::Value], title: &str) -> serde_json::Value {
    actions
        .iter()
        .find(|action| action["title"] == title)
        .unwrap_or_else(|| panic!("no action titled {title:?}"))
        .clone()
}

#[test]
fn advertises_all_twenty_two_conversions_by_default() {
    let uri = "untitled:Untitled-1";
    let mut server = Server::start(serde_json::json!({}));
    server.open(uri, "HELLO");

    let actions = server.code_actions(2, uri, range((0, 0), (0, 5)));
    assert_eq!(actions.len(), 22, "11 codepages x 2 directions");
    for action in &actions {
        assert_eq!(action["kind"], "refactor.rewrite");
        assert!(
            action["edit"].is_null(),
            "edits must be deferred until resolve"
        );
    }
    server.shutdown();
}

#[test]
fn honours_the_codepage_setting_from_initialization_options() {
    let uri = "untitled:Untitled-1";
    let mut server = Server::start(serde_json::json!({ "codepages": ["0037"] }));
    server.open(uri, "HELLO");

    let actions = server.code_actions(2, uri, range((0, 0), (0, 5)));
    let titles: Vec<&str> = actions
        .iter()
        .map(|a| a["title"].as_str().unwrap())
        .collect();
    assert_eq!(
        titles,
        ["Convert Ebcdic0037 to Ascii", "Convert Ascii to Ebcdic0037"]
    );
    server.shutdown();
}

#[test]
fn resolving_an_action_returns_an_edit_for_the_selection() {
    let uri = "untitled:Untitled-1";
    let mut server = Server::start(serde_json::json!({ "codepages": ["0037"] }));
    server.open(uri, "xxHELLOxx");

    let selection = range((0, 2), (0, 7));
    let actions = server.code_actions(2, uri, selection.clone());
    let resolved = server.resolve(3, &find(&actions, "Convert Ascii to Ebcdic0037"));

    let edits = resolved["edit"]["changes"][uri]
        .as_array()
        .expect("edits for the document");
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0]["range"], selection, "replaces only the selection");

    let encoded: Vec<u32> = edits[0]["newText"]
        .as_str()
        .unwrap()
        .chars()
        .map(u32::from)
        .collect();
    assert_eq!(
        encoded,
        [0xC8, 0xC5, 0xD3, 0xD3, 0xD6],
        "HELLO in EBCDIC 037"
    );
    server.shutdown();
}

#[test]
fn empty_range_resolves_to_a_whole_document_edit() {
    let uri = "untitled:Untitled-1";
    let mut server = Server::start(serde_json::json!({ "codepages": ["0037"] }));
    server.open(uri, "AB\nCD");

    // A bare cursor: upstream's rule is that this converts the entire document.
    let actions = server.code_actions(2, uri, range((1, 1), (1, 1)));
    let resolved = server.resolve(3, &find(&actions, "Convert Ascii to Ebcdic0037"));

    let edits = resolved["edit"]["changes"][uri].as_array().expect("edits");
    assert_eq!(
        edits[0]["range"],
        range((0, 0), (1, 2)),
        "spans the whole document"
    );
    server.shutdown();
}

#[test]
fn decodes_a_real_ebcdic_file_from_disk() {
    // The case that motivates the whole design: the buffer Zed shows for these bytes is
    // mojibake, so a correct decode has to come from the file itself.
    let directory = std::env::temp_dir().join("ebcdic-lsp-protocol-test");
    std::fs::create_dir_all(&directory).expect("create temp dir");
    let path = directory.join("hello.dat");
    std::fs::write(&path, [0xC8u8, 0xC5, 0xD3, 0xD3, 0xD6]).expect("write ebcdic bytes");
    let uri = format!("file://{}", path.display());

    let mut server = Server::start(serde_json::json!({ "codepages": ["0037"] }));
    // What a UTF-8 decoder produces for those bytes, which is what the editor holds.
    server.open(
        &uri,
        &String::from_utf8_lossy([0xC8u8, 0xC5, 0xD3, 0xD3, 0xD6].as_ref()),
    );

    let actions = server.code_actions(2, &uri, range((0, 0), (0, 0)));
    let resolved = server.resolve(3, &find(&actions, "Convert Ebcdic0037 to Ascii"));

    let edits = resolved["edit"]["changes"][&uri].as_array().expect("edits");
    assert_eq!(
        edits[0]["newText"], "HELLO",
        "decoded from disk, not from the mangled buffer"
    );

    server.shutdown();
    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn tracks_buffer_changes_before_converting() {
    let uri = "untitled:Untitled-1";
    let mut server = Server::start(serde_json::json!({ "codepages": ["0037"] }));
    server.open(uri, "WRONG");
    server.notify(
        "textDocument/didChange",
        serde_json::json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": "HELLO" }]
        }),
    );

    let actions = server.code_actions(2, uri, range((0, 0), (0, 0)));
    let resolved = server.resolve(3, &find(&actions, "Convert Ascii to Ebcdic0037"));
    let edits = resolved["edit"]["changes"][uri].as_array().expect("edits");
    let encoded: Vec<u32> = edits[0]["newText"]
        .as_str()
        .unwrap()
        .chars()
        .map(u32::from)
        .collect();
    assert_eq!(
        encoded,
        [0xC8, 0xC5, 0xD3, 0xD3, 0xD6],
        "converted the updated buffer"
    );
    server.shutdown();
}

#[test]
fn applies_configuration_changes_at_runtime() {
    let uri = "untitled:Untitled-1";
    let mut server = Server::start(serde_json::json!({}));
    server.open(uri, "HELLO");
    assert_eq!(server.code_actions(2, uri, range((0, 0), (0, 5))).len(), 22);

    server.notify(
        "workspace/didChangeConfiguration",
        serde_json::json!({ "settings": { "codepages": ["0037", "1047"] } }),
    );
    assert_eq!(server.code_actions(3, uri, range((0, 0), (0, 5))).len(), 4);
    server.shutdown();
}

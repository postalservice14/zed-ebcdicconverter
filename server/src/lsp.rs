//! LSP plumbing: capability advertisement, document tracking, and the code actions that
//! carry the conversions.
//!
//! Actions are advertised without an edit and filled in on `codeAction/resolve`. Zed refreshes
//! code actions as the cursor moves, so computing all 22 conversions up front would re-convert
//! the whole file on every cursor movement; resolving lazily converts only what was chosen.

use std::error::Error;

use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOptions, CodeActionOrCommand, CodeActionParams,
    CodeActionProviderCapability, CodeActionResponse, DidChangeConfigurationParams,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    InitializeParams, InitializeResult, MessageType, Range, ServerCapabilities, ServerInfo,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextEdit, Uri, WorkspaceEdit,
};
use serde::{Deserialize, Serialize};

use crate::config::Settings;
use crate::convert::{self, Direction, Source};
use crate::document::{self, Documents};
use crate::tables::Codepage;

const SERVER_NAME: &str = "ebcdic-lsp";

/// Payload attached to an unresolved code action, so `codeAction/resolve` can recreate the
/// conversion without re-deriving anything. The document URI must ride along: resolve requests
/// carry the action back, not the text document it came from.
#[derive(Debug, Serialize, Deserialize)]
struct ActionData {
    uri: Uri,
    codepage: String,
    direction: String,
    range: Range,
    /// True when the action applies to the whole document because nothing was selected.
    whole_document: bool,
}

pub fn run() -> Result<(), Box<dyn Error + Sync + Send>> {
    let (connection, io_threads) = Connection::stdio();

    let (request_id, initialize_params) = connection.initialize_start()?;
    let initialize_params: InitializeParams = serde_json::from_value(initialize_params)?;

    let settings = initialize_params
        .initialization_options
        .as_ref()
        .map(Settings::from_value)
        .unwrap_or_default();

    let initialize_result = InitializeResult {
        capabilities: capabilities(),
        server_info: Some(ServerInfo {
            name: SERVER_NAME.to_string(),
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
        }),
    };
    connection.initialize_finish(request_id, serde_json::to_value(initialize_result)?)?;

    let mut server = Server::new(settings);
    server.warn_about_unknown_codepages(&connection);
    server.serve(&connection)?;

    // Drop the connection before joining: it owns the sender half of the writer thread's
    // channel, and that thread runs until the channel closes. Joining while `connection` is
    // still alive deadlocks, leaving a stray server process behind after every editor session.
    drop(connection);
    io_threads.join()?;
    Ok(())
}

fn capabilities() -> ServerCapabilities {
    ServerCapabilities {
        // Full sync: the documents are small and full text keeps position arithmetic honest.
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        code_action_provider: Some(CodeActionProviderCapability::Options(CodeActionOptions {
            code_action_kinds: Some(vec![CodeActionKind::REFACTOR_REWRITE]),
            resolve_provider: Some(true),
            work_done_progress_options: Default::default(),
        })),
        ..Default::default()
    }
}

struct Server {
    documents: Documents,
    settings: Settings,
}

impl Server {
    fn new(settings: Settings) -> Self {
        Self {
            documents: Documents::new(),
            settings,
        }
    }

    fn serve(&mut self, connection: &Connection) -> Result<(), Box<dyn Error + Sync + Send>> {
        for message in &connection.receiver {
            match message {
                Message::Request(request) => {
                    if connection.handle_shutdown(&request)? {
                        return Ok(());
                    }
                    let response = self.handle_request(request);
                    connection.sender.send(Message::Response(response))?;
                }
                Message::Notification(notification) => self.handle_notification(notification),
                // Responses only arrive for server-initiated requests, and we send none.
                Message::Response(_) => {}
            }
        }
        Ok(())
    }

    fn handle_request(&mut self, request: Request) -> Response {
        let id = request.id.clone();
        match request.method.as_str() {
            "textDocument/codeAction" => match cast::<CodeActionParams>(request) {
                Ok(params) => success(id, self.code_actions(&params)),
                Err(message) => Response::new_err(id, INVALID_PARAMS, message),
            },
            "codeAction/resolve" => match cast::<CodeAction>(request) {
                Ok(action) => success(id, self.resolve(action)),
                Err(message) => Response::new_err(id, INVALID_PARAMS, message),
            },
            _ => Response::new_err(
                id,
                METHOD_NOT_FOUND,
                format!("unsupported method: {}", request.method),
            ),
        }
    }

    fn handle_notification(&mut self, notification: Notification) {
        match notification.method.as_str() {
            "textDocument/didOpen" => {
                if let Ok(params) = parse::<DidOpenTextDocumentParams>(notification) {
                    self.documents
                        .open(&params.text_document.uri, params.text_document.text);
                }
            }
            "textDocument/didChange" => {
                if let Ok(params) = parse::<DidChangeTextDocumentParams>(notification) {
                    // Full sync means the last change carries the entire document.
                    if let Some(change) = params.content_changes.into_iter().last() {
                        self.documents
                            .update(&params.text_document.uri, change.text);
                    }
                }
            }
            "textDocument/didClose" => {
                if let Ok(params) = parse::<DidCloseTextDocumentParams>(notification) {
                    self.documents.close(&params.text_document.uri);
                }
            }
            "workspace/didChangeConfiguration" => {
                if let Ok(params) = parse::<DidChangeConfigurationParams>(notification) {
                    self.settings = Settings::from_value(&params.settings);
                }
            }
            _ => {}
        }
    }

    /// Build one unresolved action per enabled codepage and direction.
    fn code_actions(&self, params: &CodeActionParams) -> CodeActionResponse {
        let uri = &params.text_document.uri;
        let Some(text) = self.documents.get(uri) else {
            return Vec::new();
        };

        let whole_document = document::is_empty(params.range);
        let range = if whole_document {
            document::full_range(text)
        } else {
            params.range
        };

        // An empty document has nothing to convert, and offering 22 no-op actions on every
        // empty buffer would be pure noise.
        if document::slice(text, range).is_none_or(str::is_empty) {
            return Vec::new();
        }

        let mut actions = Vec::new();
        for codepage in self.settings.enabled_codepages() {
            for direction in [Direction::EbcdicToAscii, Direction::AsciiToEbcdic] {
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: direction.title(codepage.id),
                    kind: Some(CodeActionKind::REFACTOR_REWRITE),
                    data: serde_json::to_value(ActionData {
                        uri: uri.clone(),
                        codepage: codepage.id.to_string(),
                        direction: direction.as_str().to_string(),
                        range,
                        whole_document,
                    })
                    .ok(),
                    ..Default::default()
                }));
            }
        }
        actions
    }

    /// Compute the edit for the action the user actually picked.
    fn resolve(&self, mut action: CodeAction) -> CodeAction {
        let Some(data) = action.data.take() else {
            return action;
        };
        let Ok(data) = serde_json::from_value::<ActionData>(data) else {
            return action;
        };
        let Some(direction) = Direction::from_str(&data.direction) else {
            return action;
        };
        let Some(codepage) = self.codepage(&data.codepage) else {
            return action;
        };
        let Some(text) = self.documents.get(&data.uri) else {
            return action;
        };

        let source = self.source_for(&data, direction, text);
        let Some(source) = source else { return action };

        let new_text = convert::convert(&source, direction, codepage);
        action.edit = Some(WorkspaceEdit {
            changes: Some(
                [(
                    data.uri.clone(),
                    vec![TextEdit {
                        range: data.range,
                        new_text,
                    }],
                )]
                .into_iter()
                .collect(),
            ),
            ..Default::default()
        });
        action
    }

    /// Decide what to feed the converter.
    ///
    /// Reading the file from disk for a whole-document EBCDIC decode is the crux of this
    /// server, and matches upstream. A genuine EBCDIC file is not valid UTF-8, so the editor
    /// has already replaced its invalid sequences with U+FFFD; converting the buffer would
    /// convert that damage instead of the data. Selections have no byte range to read (the
    /// mapping from buffer positions back to file offsets is not recoverable once bytes have
    /// been replaced), so they use the buffer text.
    fn source_for<'a>(
        &self,
        data: &ActionData,
        direction: Direction,
        text: &'a str,
    ) -> Option<Source<'a>> {
        if direction == Direction::EbcdicToAscii && data.whole_document {
            if let Some(path) = document::file_path(&data.uri) {
                if let Ok(bytes) = std::fs::read(&path) {
                    return Some(Source::Bytes(bytes));
                }
            }
        }
        document::slice(text, data.range).map(Source::Text)
    }

    fn codepage(&self, id: &str) -> Option<&'static Codepage> {
        self.settings
            .enabled_codepages()
            .into_iter()
            .find(|codepage| codepage.id == id)
    }

    /// Tell the user about codepage ids that match nothing, so a typo in settings is visible
    /// rather than silently reducing the menu.
    fn warn_about_unknown_codepages(&self, connection: &Connection) {
        let unknown = self.settings.unknown_codepages();
        if unknown.is_empty() {
            return;
        }
        let message = format!(
            "ebcdic-lsp: ignoring unknown codepage(s): {}. Known codepages: {}.",
            unknown.join(", "),
            crate::tables::CODEPAGES
                .iter()
                .map(|codepage| codepage.id)
                .collect::<Vec<_>>()
                .join(", ")
        );
        let _ = connection
            .sender
            .send(Message::Notification(Notification::new(
                "window/logMessage".to_string(),
                lsp_types::LogMessageParams {
                    typ: MessageType::WARNING,
                    message,
                },
            )));
    }
}

const INVALID_PARAMS: i32 = ErrorCode::InvalidParams as i32;
const METHOD_NOT_FOUND: i32 = ErrorCode::MethodNotFound as i32;

fn success<T: Serialize>(id: RequestId, value: T) -> Response {
    match serde_json::to_value(value) {
        Ok(value) => Response::new_ok(id, value),
        Err(error) => Response::new_err(id, ErrorCode::InternalError as i32, error.to_string()),
    }
}

fn cast<T: serde::de::DeserializeOwned>(request: Request) -> Result<T, String> {
    serde_json::from_value(request.params).map_err(|error| error.to_string())
}

fn parse<T: serde::de::DeserializeOwned>(notification: Notification) -> Result<T, String> {
    serde_json::from_value(notification.params).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{
        CodeActionContext, PartialResultParams, Position, TextDocumentIdentifier,
        WorkDoneProgressParams,
    };
    use serde_json::json;

    fn uri(text: &str) -> Uri {
        text.parse().expect("valid uri")
    }

    fn range(start: (u32, u32), end: (u32, u32)) -> Range {
        Range {
            start: Position {
                line: start.0,
                character: start.1,
            },
            end: Position {
                line: end.0,
                character: end.1,
            },
        }
    }

    fn params(document: &Uri, selection: Range) -> CodeActionParams {
        CodeActionParams {
            text_document: TextDocumentIdentifier {
                uri: document.clone(),
            },
            range: selection,
            context: CodeActionContext::default(),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        }
    }

    /// A server with one open document.
    fn server_with(document: &Uri, text: &str, settings: serde_json::Value) -> Server {
        let mut server = Server::new(Settings::from_value(&settings));
        server.documents.open(document, text.to_string());
        server
    }

    fn titles(actions: &CodeActionResponse) -> Vec<String> {
        actions
            .iter()
            .map(|action| match action {
                CodeActionOrCommand::CodeAction(action) => action.title.clone(),
                CodeActionOrCommand::Command(command) => command.title.clone(),
            })
            .collect()
    }

    fn only_action(actions: CodeActionResponse, title: &str) -> CodeAction {
        actions
            .into_iter()
            .filter_map(|action| match action {
                CodeActionOrCommand::CodeAction(action) => Some(action),
                CodeActionOrCommand::Command(_) => None,
            })
            .find(|action| action.title == title)
            .unwrap_or_else(|| panic!("no action titled {title:?}"))
    }

    // `Uri` keys trip clippy's mutable_key_type; the map comes from lsp-types and is read-only
    // here, so there is no mutation hazard to guard against.
    #[allow(clippy::mutable_key_type)]
    fn edit_of(action: &CodeAction) -> (Range, String) {
        let changes = action
            .edit
            .as_ref()
            .expect("resolved action has an edit")
            .changes
            .as_ref()
            .expect("edit has changes");
        let edits = changes.values().next().expect("one document edited");
        assert_eq!(edits.len(), 1, "expected exactly one text edit");
        (edits[0].range, edits[0].new_text.clone())
    }

    #[test]
    fn offers_two_actions_per_codepage() {
        let document = uri("file:///tmp/a.txt");
        let server = server_with(&document, "HELLO", json!({}));
        let actions = server.code_actions(&params(&document, range((0, 0), (0, 5))));
        assert_eq!(actions.len(), 22, "11 codepages x 2 directions");
        assert!(titles(&actions).contains(&"Convert Ebcdic0037 to Ascii".to_string()));
        assert!(titles(&actions).contains(&"Convert Ascii to Ebcdic1047".to_string()));
    }

    #[test]
    fn codepage_setting_narrows_the_menu() {
        let document = uri("file:///tmp/a.txt");
        let server = server_with(&document, "HELLO", json!({ "codepages": ["0037"] }));
        let actions = server.code_actions(&params(&document, range((0, 0), (0, 5))));
        assert_eq!(
            titles(&actions),
            ["Convert Ebcdic0037 to Ascii", "Convert Ascii to Ebcdic0037"]
        );
    }

    #[test]
    fn actions_are_advertised_without_an_edit() {
        // The edit must arrive only on resolve; otherwise every cursor move converts the file.
        let document = uri("file:///tmp/a.txt");
        let server = server_with(&document, "HELLO", json!({ "codepages": ["0037"] }));
        let actions = server.code_actions(&params(&document, range((0, 0), (0, 5))));
        for action in &actions {
            let CodeActionOrCommand::CodeAction(action) = action else {
                panic!("expected a CodeAction");
            };
            assert!(action.edit.is_none(), "edit must be deferred to resolve");
            assert!(action.data.is_some(), "resolve needs the data payload");
            assert_eq!(action.kind, Some(CodeActionKind::REFACTOR_REWRITE));
        }
    }

    #[test]
    fn no_actions_for_an_unknown_or_empty_document() {
        let document = uri("file:///tmp/a.txt");
        let server = Server::new(Settings::default());
        assert!(server
            .code_actions(&params(&document, range((0, 0), (0, 1))))
            .is_empty());

        let empty = server_with(&document, "", json!({}));
        assert!(empty
            .code_actions(&params(&document, range((0, 0), (0, 0))))
            .is_empty());
    }

    #[test]
    fn selection_is_converted_in_place() {
        let document = uri("file:///tmp/a.txt");
        let server = server_with(&document, "xxHELLOxx", json!({ "codepages": ["0037"] }));
        let selection = range((0, 2), (0, 7));
        let actions = server.code_actions(&params(&document, selection));
        let resolved = server.resolve(only_action(actions, "Convert Ascii to Ebcdic0037"));

        let (edited_range, new_text) = edit_of(&resolved);
        assert_eq!(edited_range, selection, "only the selection is replaced");
        let bytes: Vec<u32> = new_text.chars().map(u32::from).collect();
        assert_eq!(bytes, [0xC8, 0xC5, 0xD3, 0xD3, 0xD6], "HELLO in EBCDIC 037");
    }

    #[test]
    fn empty_selection_converts_the_whole_document() {
        let document = uri("untitled:Untitled-1");
        let server = server_with(&document, "AB\nCD", json!({ "codepages": ["0037"] }));
        // A bare cursor part-way through the document.
        let actions = server.code_actions(&params(&document, range((1, 1), (1, 1))));
        let resolved = server.resolve(only_action(actions, "Convert Ascii to Ebcdic0037"));

        let (edited_range, new_text) = edit_of(&resolved);
        assert_eq!(
            edited_range,
            range((0, 0), (1, 2)),
            "spans the whole document"
        );
        let bytes: Vec<u32> = new_text.chars().map(u32::from).collect();
        assert_eq!(bytes, [0xC1, 0xC2, 0x25, 0xC3, 0xC4], "AB, newline, CD");
    }

    #[test]
    fn whole_document_decode_reads_bytes_from_disk() {
        // The reason this server exists: the buffer has already lost the original bytes to
        // U+FFFD, so a correct decode must come from the file.
        let directory = std::env::temp_dir().join("ebcdic-lsp-test-disk");
        std::fs::create_dir_all(&directory).expect("create temp dir");
        let path = directory.join("hello.dat");
        std::fs::write(&path, [0xC8u8, 0xC5, 0xD3, 0xD3, 0xD6]).expect("write ebcdic file");

        let document = uri(&format!("file://{}", path.display()));
        // Mojibake stands in for what Zed actually shows for these bytes.
        let server = server_with(
            &document,
            "\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}",
            json!({ "codepages": ["0037"] }),
        );
        let actions = server.code_actions(&params(&document, range((0, 0), (0, 0))));
        let resolved = server.resolve(only_action(actions, "Convert Ebcdic0037 to Ascii"));

        let (_, new_text) = edit_of(&resolved);
        assert_eq!(
            new_text, "HELLO",
            "decoded from disk bytes, not the mangled buffer"
        );

        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn selection_decode_uses_buffer_text_not_disk() {
        // Selections have no recoverable byte range, so they must convert what is on screen
        // even when the file exists on disk.
        let directory = std::env::temp_dir().join("ebcdic-lsp-test-selection");
        std::fs::create_dir_all(&directory).expect("create temp dir");
        let path = directory.join("hello.dat");
        std::fs::write(&path, [0xC8u8, 0xC5, 0xD3, 0xD3, 0xD6]).expect("write ebcdic file");

        let document = uri(&format!("file://{}", path.display()));
        // Buffer holds characters whose scalars are EBCDIC bytes for "HI".
        let buffer = format!("{}{}", char::from(0xC8u8), char::from(0xC9u8));
        let server = server_with(&document, &buffer, json!({ "codepages": ["0037"] }));
        let actions = server.code_actions(&params(&document, range((0, 0), (0, 2))));
        let resolved = server.resolve(only_action(actions, "Convert Ebcdic0037 to Ascii"));

        let (_, new_text) = edit_of(&resolved);
        assert_eq!(new_text, "HI", "converted the selection from the buffer");

        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn untitled_document_decode_falls_back_to_buffer_text() {
        let document = uri("untitled:Untitled-1");
        let buffer = format!("{}{}", char::from(0xC8u8), char::from(0xC9u8));
        let server = server_with(&document, &buffer, json!({ "codepages": ["0037"] }));
        let actions = server.code_actions(&params(&document, range((0, 0), (0, 0))));
        let resolved = server.resolve(only_action(actions, "Convert Ebcdic0037 to Ascii"));

        assert_eq!(edit_of(&resolved).1, "HI");
    }

    #[test]
    fn resolve_without_usable_data_returns_the_action_unchanged() {
        let server = Server::new(Settings::default());
        for data in [
            None,
            Some(json!("nonsense")),
            Some(json!({ "uri": "file:///x" })),
        ] {
            let action = CodeAction {
                title: "Convert Ebcdic0037 to Ascii".to_string(),
                data,
                ..Default::default()
            };
            assert!(
                server.resolve(action).edit.is_none(),
                "must not fabricate an edit"
            );
        }
    }

    #[test]
    fn round_trip_through_two_resolved_actions_restores_the_text() {
        let document = uri("untitled:Untitled-1");
        let original = "The quick brown fox!";
        let server = server_with(&document, original, json!({ "codepages": ["1047"] }));

        let encoded = edit_of(&server.resolve(only_action(
            server.code_actions(&params(&document, range((0, 0), (0, 0)))),
            "Convert Ascii to Ebcdic1047",
        )))
        .1;

        // Feed the encoded text back in as the buffer contents and decode it again.
        let mut second = server_with(&document, &encoded, json!({ "codepages": ["1047"] }));
        second.settings = Settings::from_value(&json!({ "codepages": ["1047"] }));
        let decoded = edit_of(&second.resolve(only_action(
            second.code_actions(&params(&document, range((0, 0), (0, 0)))),
            "Convert Ebcdic1047 to Ascii",
        )))
        .1;

        assert_eq!(decoded, original);
    }

    #[test]
    fn configuration_change_updates_the_offered_codepages() {
        let document = uri("file:///tmp/a.txt");
        let mut server = server_with(&document, "HELLO", json!({}));
        assert_eq!(
            server
                .code_actions(&params(&document, range((0, 0), (0, 5))))
                .len(),
            22
        );

        server.handle_notification(Notification::new(
            "workspace/didChangeConfiguration".to_string(),
            DidChangeConfigurationParams {
                settings: json!({ "codepages": ["0037"] }),
            },
        ));
        assert_eq!(
            server
                .code_actions(&params(&document, range((0, 0), (0, 5))))
                .len(),
            2
        );
    }

    #[test]
    fn document_lifecycle_notifications_are_tracked() {
        let document = uri("file:///tmp/a.txt");
        let mut server = Server::new(Settings::default());

        server.handle_notification(Notification::new(
            "textDocument/didOpen".to_string(),
            json!({ "textDocument": {
                "uri": document.as_str(), "languageId": "plaintext", "version": 1, "text": "one"
            }}),
        ));
        assert_eq!(server.documents.get(&document), Some("one"));

        server.handle_notification(Notification::new(
            "textDocument/didChange".to_string(),
            json!({
                "textDocument": { "uri": document.as_str(), "version": 2 },
                "contentChanges": [{ "text": "two" }]
            }),
        ));
        assert_eq!(server.documents.get(&document), Some("two"));

        server.handle_notification(Notification::new(
            "textDocument/didClose".to_string(),
            json!({ "textDocument": { "uri": document.as_str() } }),
        ));
        assert_eq!(server.documents.get(&document), None);
    }

    #[test]
    fn capabilities_advertise_resolvable_rewrite_actions() {
        let capabilities = capabilities();
        let Some(CodeActionProviderCapability::Options(options)) =
            capabilities.code_action_provider
        else {
            panic!("expected code action options");
        };
        assert_eq!(options.resolve_provider, Some(true));
        assert_eq!(
            options.code_action_kinds,
            Some(vec![CodeActionKind::REFACTOR_REWRITE])
        );
        assert!(matches!(
            capabilities.text_document_sync,
            Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL))
        ));
    }

    #[test]
    fn unsupported_request_gets_a_method_not_found_error() {
        let mut server = Server::new(Settings::default());
        let response = server.handle_request(Request::new(
            1.into(),
            "textDocument/formatting".to_string(),
            json!({}),
        ));
        let error = response
            .response_result
            .expect_err("expected an error response");
        assert_eq!(error.code, METHOD_NOT_FOUND);
    }
}

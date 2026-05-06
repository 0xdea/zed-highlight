#![doc = include_str!("../../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/0xdea/zed-highlight/master/.img/logo.png"
)]
#![allow(
    clippy::wildcard_imports,
    reason = "Implicit import of all LSP types is more convenient than listing them one by one."
)]

use std::collections::HashMap;
use std::sync::Arc;

use regex::Regex;
use tokio::sync::Mutex;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

/// Package name
const PROGRAM: &str = env!("CARGO_PKG_NAME");
/// Package version
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// How many distinct highlight colors to cycle through.
const NUM_COLORS: usize = 8;

/// These names are arbitrary strings that the LSP server advertises as its semantic token type legend. Zed looks them
/// up in `global_lsp_settings.semantic_token_rules` (settings.json) to map each name to a foreground/background color.
///
/// ## Examples
///
/// `global_lsp_settings` snippet for the dark theme (8 highlight colors with 50% opacity backgrounds):
/// ```json
/// "global_lsp_settings": {
///   "semantic_token_rules": [
///     { "token_type": "highlight-0", "foreground_color": "#F5B041", "background_color": "#F5B04150" },
///     { "token_type": "highlight-1", "foreground_color": "#85C1E9", "background_color": "#85C1E950" },
///     { "token_type": "highlight-2", "foreground_color": "#CD6155", "background_color": "#CD615550" },
///     { "token_type": "highlight-3", "foreground_color": "#AF7AC5", "background_color": "#AF7AC550" },
///     { "token_type": "highlight-4", "foreground_color": "#48C9B0", "background_color": "#48C9B050" },
///     { "token_type": "highlight-5", "foreground_color": "#F4D03F", "background_color": "#F4D03F50" },
///     { "token_type": "highlight-6", "foreground_color": "#52BE80", "background_color": "#52BE8050" },
///     { "token_type": "highlight-7", "foreground_color": "#FF9933", "background_color": "#FF993350" },
///   ],
/// },
/// ```
static TOKEN_TYPE_NAMES: [&str; NUM_COLORS] = [
    "highlight-0",
    "highlight-1",
    "highlight-2",
    "highlight-3",
    "highlight-4",
    "highlight-5",
    "highlight-6",
    "highlight-7",
];

/// The LSP server's internal state.
///
/// We track the list of highlighted words and the full text of every open document so we can scan for token positions
/// on demand. We also track the user's settings for whole-word and case-insensitive matching. All state is kept in
/// memory and shared across all documents and tabs, so highlighting a word in one file highlights it in all files.
struct State {
    /// The list of currently highlighted words. A removed slot becomes `None` and subsequent entries are not shifted.
    words: Vec<Option<String>>,

    /// Full text of every open document, keyed by URI.
    docs: HashMap<Url, String>,

    /// Whether to only match whole words (e.g., "for" doesn't match inside "format"). Default: true.
    whole_word: bool,

    /// Whether to ignore case when matching (e.g., "Foo" matches "foo"). Default: true.
    ignore_case: bool,
}

impl State {
    /// Construct and return a new instance of the server's internal state.
    fn new() -> Self {
        Self {
            words: Vec::new(),
            docs: HashMap::new(),
            whole_word: true,
            ignore_case: true,
        }
    }

    /// Toggle a word in/out of the highlight list.
    ///
    /// Three cases:
    /// 1. Word already exists: soft-delete it preserving the slot (set its slot to `None`).
    /// 2. Word is new and there are `None` slots: reuse the first free slot so we reclaim the color index.
    /// 3. Word is new and there are no free slots: grow the list appending a new `Some(word)`.
    fn toggle(&mut self, word: &str) {
        let ignore_case = self.ignore_case;

        // Build a comparison closure once so we don't repeat the branch inside the iterator.
        let eq = move |a: &str, b: &str| -> bool {
            if ignore_case {
                a.to_lowercase() == b.to_lowercase()
            } else {
                a == b
            }
        };

        if let Some(idx) = self
            .words
            .iter()
            .position(|opt| opt.as_deref().is_some_and(|w| eq(w, word)))
        {
            // Case 1: Word already exists: soft-delete it preserving the slot (set its slot to `None`).
            self.words[idx] = None;
        } else if let Some(slot) = self.words.iter_mut().find(|o| o.is_none()) {
            // Case 2: Word is new and there are `None` slots: reuse the first free slot so we reclaim the color index.
            *slot = Some(word.to_owned());
        } else {
            // Case 3: Word is new and there are no free slots: grow the list appending a new `Some(word)`.
            self.words.push(Some(word.to_owned()));
        }
    }

    /// Check whether a word is currently highlighted, respecting `ignore_case`.
    fn is_highlighted(&self, word: &str) -> bool {
        let ignore_case = self.ignore_case;
        self.words.iter().any(|opt| {
            opt.as_deref().is_some_and(|w| {
                if ignore_case {
                    w.to_lowercase() == word.to_lowercase()
                } else {
                    w == word
                }
            })
        })
    }

    /// True if at least one word is currently highlighted. Used to skip refresh when the highlight list is empty.
    fn has_any(&self) -> bool {
        self.words.iter().any(std::option::Option::is_some)
    }
}

/// The LSP server's backend implementation.
///
/// This struct holds the shared state and implements the `LanguageServer` trait that tower-lsp dispatches to. Each
/// method corresponds to a particular LSP request or notification that Zed sends us. The main logic is in
/// `build_tokens`, which scans the document for matches and encodes the token positions in the format required by the
/// LSP semantic tokens protocol.
struct Backend {
    /// The tower-lsp client handle used to send server->client notifications.
    client: Client,

    /// Shared mutable state behind a tokio async `Mutex`.
    state: Arc<Mutex<State>>,

    /// Handle to the currently pending debounced-refresh task, if any.
    refresh_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Backend {
    /// Construct and return a new instance of the server's baxckend.
    fn new(client: Client) -> Self {
        Self {
            client,
            state: Arc::new(Mutex::new(State::new())),
            refresh_handle: Mutex::new(None),
        }
    }

    /// Cancel any pending debounced refresh and send a workspace/semanticTokens/refresh notification to Zed right now.
    /// Used after user-triggered actions (toggle/clear) where we want the highlight change to appear without delay.
    async fn immediate_refresh(&self) {
        let refresh_handle = self.refresh_handle.lock().await.take();
        if let Some(h) = refresh_handle {
            h.abort();
        }
        self.client.semantic_tokens_refresh().await.ok();
    }

    /// Schedule a workspace/semanticTokens/refresh after a short idle delay, cancelling any previously scheduled one.
    /// This is a classic debounce: rapid events (keystrokes) keep resetting the timer; the refresh only fires
    /// once the user pauses.
    async fn debounced_refresh(&self) {
        let mut guard = self.refresh_handle.lock().await;
        // Abort the previous timer task if one is already running.
        if let Some(h) = guard.take() {
            h.abort();
        }
        let client = self.client.clone();
        *guard = Some(tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            client.semantic_tokens_refresh().await.ok();
        }));
    }

    /// Build the full list of `SemanticTokens` for a document.
    ///
    /// The LSP semantic-tokens protocol requires tokens to be encoded as a flat array of 5-tuples in document order,
    /// where each position is expressed as a delta from the previous token (not an absolute position). This lets
    /// the client decode the stream in one pass without random access.
    ///
    /// Character offsets must be in UTF-16 code units because that is what the LSP spec mandates.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "UTF-16 code unit count should fit in u32 for any reasonable line length."
    )]
    async fn build_tokens(&self, uri: &Url) -> Vec<SemanticToken> {
        let (content, words, whole_word, ignore_case) = {
            let state = self.state.lock().await;
            let Some(content) = state.docs.get(uri).cloned() else {
                // Document not yet registered: return empty; the debounced refresh will fix it.
                return vec![];
            };
            (
                content,
                state.words.clone(),
                state.whole_word,
                state.ignore_case,
            )
        };

        // Collect all matches as absolute (line, char_start, char_len, type_idx) tuples first, then sort, then convert
        // to delta encoding. Doing it in two passes is easier than trying to maintain sorted order while iterating
        // over multiple words.
        let mut raw: Vec<(u32, u32, u32, u32)> = Vec::new();

        for (color_idx, opt) in words.iter().enumerate() {
            let word = match opt {
                Some(w) if !w.is_empty() => w,
                // Skip None (soft-deleted) and empty-string slots.
                _ => continue,
            };

            // Build the regex for this word. We escape the word first so that punctuation in the word is treated
            // literally, not as regex syntax.
            let escaped = regex::escape(word);
            let mut pattern = if whole_word {
                // The word-boundary assertion prevents "for" from matching inside "format".
                format!(r"\b{escaped}\b")
            } else {
                escaped
            };
            if ignore_case {
                // Make the entire pattern case-insensitive.
                pattern = format!("(?i){pattern}");
            }

            let Ok(re) = Regex::new(&pattern) else {
                continue;
            };

            // Color index wraps around if more than NUM_COLORS words are highlighted simultaneously.
            let type_idx = (color_idx % NUM_COLORS) as u32;

            for (line_idx, line) in content.lines().enumerate() {
                for m in re.find_iter(line) {
                    // The LSP protocol requires UTF-16 character offsets, so we convert.
                    let char_start = utf16_len(&line[..m.start()]);
                    let char_len = utf16_len(m.as_str());
                    raw.push((line_idx as u32, char_start, char_len, type_idx));
                }
            }
        }

        // Sort by (line, start) so we can compute deltas in a single forward pass. Multiple words can produce matches
        // on the same line, so we must sort across all of them together.
        raw.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

        // Convert absolute positions to the LSP delta encoding.
        // Rules from the spec:
        // delta_line  = this_line - prev_line
        // delta_start = this_start - prev_start   (only when delta_line == 0)
        //             = this_start                (when delta_line > 0)
        // i.e., the start offset resets to absolute whenever the line changes.
        let mut tokens = Vec::with_capacity(raw.len());
        let mut prev_line = 0_u32;
        let mut prev_start = 0_u32;

        for (line, start, length, token_type) in raw {
            let delta_line = line - prev_line;
            let delta_start = if delta_line == 0 {
                start - prev_start
            } else {
                start
            };
            tokens.push(SemanticToken {
                delta_line,
                delta_start,
                length,
                token_type,
                token_modifiers_bitset: 0, // We declare no modifiers.
            });
            prev_line = line;
            prev_start = start;
        }

        tokens
    }
}

/// Helper function to count the number of UTF-16 code units in a UTF-8 string slice.
#[expect(
    clippy::cast_possible_truncation,
    reason = "UTF-16 code unit count should fit in u32 for any reasonable line length."
)]
fn utf16_len(s: &str) -> u32 {
    s.chars().map(|c| c.len_utf16() as u32).sum()
}

/// Helper function to convert a UTF-16 character offset to a byte offset within `s`.
fn utf16_to_byte(s: &str, utf16_offset: usize) -> Option<usize> {
    let mut count = 0_usize;
    for (i, c) in s.char_indices() {
        if count == utf16_offset {
            return Some(i);
        }
        count += c.len_utf16();
    }
    // Handle the case where the offset points exactly at the end of the string.
    if count == utf16_offset {
        Some(s.len())
    // Offset is past the end (shouldn't happen with valid LSP data).
    } else {
        None
    }
}

/// Helper function to return the word the user is acting on, given the cursor range from a codeAction request.
///
/// Two cases:
/// 1. Non-empty single-line selection: use the selected text directly. This lets the user highlight multi-word phrases
///    or identifiers that include characters our word-boundary logic would split on.
/// 2. Cursor (empty range, or multi-line — we ignore multi-line): find the word under the cursor by scanning backwards
///    and forwards for word chars.
///
/// "Word characters" are alphanumerics plus underscore, matching \w in most regex flavors and covering the common case
///  of identifiers in source code.
fn word_at(content: &str, range: Range) -> Option<String> {
    let line = content.lines().nth(range.start.line as usize)?;

    // Case 1: Non-empty single-line selection.
    if range.start.line == range.end.line && range.start.character != range.end.character {
        let s = utf16_to_byte(line, range.start.character as usize)?;
        let e = utf16_to_byte(line, range.end.character as usize)?;
        if s < e {
            let text = line[s..e].trim().to_owned();
            if !text.is_empty() {
                return Some(text);
            }
        }
    }

    // Case 2: Find the word under the cursor.
    let byte_pos = utf16_to_byte(line, range.start.character as usize)?;
    let is_word = |c: char| c.is_alphanumeric() || c == '_';

    // If the cursor is sitting on a non-word character, there's nothing to highlight (e.g., cursor is on a space or
    // punctuation).
    if !line[byte_pos..].chars().next().is_some_and(is_word) {
        return None;
    }

    // Scan left from cursor_pos to find the start of the word.
    let start = line[..byte_pos]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map_or(byte_pos, |(i, _)| i);

    // Scan right from cursor_pos to find the end of the word.
    // We add the character's byte length so `end` is a past-the-end byte index.
    let end = byte_pos
        + line[byte_pos..]
            .char_indices()
            .take_while(|(_, c)| is_word(*c))
            .last()
            .map_or(0, |(i, c)| i + c.len_utf8());

    if start < end {
        Some(line[start..end].to_string())
    } else {
        None
    }
}

/// LSP server implementation.
///
/// We implement the `LanguageServer` trait from tower-lsp, which requires us to define an async method for each LSP
/// request/notification we want to handle. The `Backend` struct holds our shared state and client handle, and we
/// dispatch to helper methods for the main logic.
#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    /// Called once at server startup. We respond with our capabilities so Zed knows which features we support and how
    /// to interact with us.
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                // FULL sync: on every edit, Zed sends us the complete new document text.
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),

                // Advertise our semantic token legend. The legend is the list of token type names we will use in
                // responses; the client maps each name to a visual style. We declare no modifiers (bold, italic, etc.)
                // because we only need colors. We advertise full tokens only (not range or delta).
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: SemanticTokensLegend {
                                token_types: TOKEN_TYPE_NAMES
                                    .iter()
                                    .map(|&n| SemanticTokenType::new(n))
                                    .collect(),
                                token_modifiers: vec![],
                            },
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            range: None,
                            work_done_progress_options: WorkDoneProgressOptions::default(),
                        },
                    ),
                ),

                // Code actions appear in the editor's lightbulb / ⌘. menu. We use them to surface "Highlight: word"
                // and "Clear all" without requiring the user to bind a custom keymap entry.
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),

                // Register each supported command name so Zed knows to route `executeCommand` calls to this server.
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec!["highlight.toggle".to_owned(), "highlight.clear".to_owned()],
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                }),

                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: PROGRAM.to_owned(),
                version: Some(VERSION.to_owned()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {}

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    /// Called when Zed opens a file for the first time (not on every tab switch to an already-open file).
    ///
    /// To prevent race conditions, after storing the document we schedule a debounced refresh, which asks Zed to
    /// re-request tokens 250 ms later, by which time state.docs is guaranteed to be up to date.
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let has_any = {
            let mut state = self.state.lock().await;
            state
                .docs
                .insert(params.text_document.uri, params.text_document.text);
            state.has_any()
        };
        if has_any {
            self.debounced_refresh().await;
        }
    }

    /// Called on every edit. With FULL sync the `content_changes` vec always contains exactly one entry holding the
    /// complete new document text.
    ///
    /// Again, `debounced_refresh` is the safety net that corrects any stale token response once typing pauses.
    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let has_any = {
            let mut state = self.state.lock().await;
            if let Some(change) = params.content_changes.into_iter().last() {
                state.docs.insert(params.text_document.uri, change.text);
            }
            state.has_any()
        };
        if has_any {
            self.debounced_refresh().await;
        }
    }

    /// Called when a tab is closed. We evict the document to reclaim memory; if the file is reopened, `did_open` will
    /// re-populate `state.docs`.
    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.state
            .lock()
            .await
            .docs
            .remove(&params.text_document.uri);
    }

    /// Zed calls this whenever it wants the current highlight tokens for a file. It is called:
    /// - When the file first opens.
    /// - After each `did_change` (Zed's own auto-request),
    /// - In response to our `workspace/semanticTokens/refresh` notification.
    ///
    /// We return `result_id: None`, meaning we do not support delta tokens.
    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let data = self.build_tokens(&params.text_document.uri).await;
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        })))
    }

    /// Called whenever Zed opens the code-action menu (⌘. / lightbulb).
    /// We return up to two actions:
    /// - "Highlight: word" or "Remove highlight: word" (if cursor is on a word)
    /// - "Clear all highlights" (only if there are any active highlights)
    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let state = self.state.lock().await;
        let content = match state.docs.get(&params.text_document.uri) {
            Some(c) => c.clone(),
            None => return Ok(None),
        };
        let has_any = state.has_any();
        let word = word_at(&content, params.range);
        let already_highlighted = word.as_deref().is_some_and(|w| state.is_highlighted(w));

        // Release the lock before building the response.
        drop(state);

        let mut actions: Vec<CodeActionOrCommand> = Vec::new();

        if let Some(ref w) = word {
            let title = if already_highlighted {
                format!("Remove highlight: \"{w}\"")
            } else {
                format!("Highlight: \"{w}\"")
            };
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::EMPTY),
                // The `Command` is embedded in the `CodeAction` and passed back verbatim to `execute_command` when the
                // user selects this item. We encode the word as the single argument so `execute_command` can toggle it
                // without re-reading the cursor position.
                command: Some(Command {
                    title: "Toggle Highlight".to_owned(),
                    command: "highlight.toggle".to_owned(),
                    arguments: Some(vec![serde_json::Value::String(w.clone())]),
                }),
                ..Default::default()
            }));
        }

        if has_any {
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: "Clear all highlights".to_owned(),
                kind: Some(CodeActionKind::EMPTY),
                command: Some(Command {
                    title: "Clear All Highlights".to_owned(),
                    command: "highlight.clear".to_owned(),
                    arguments: None,
                }),
                ..Default::default()
            }));
        }

        Ok(Some(actions))
    }

    /// Called when the user selects a code action. We mutate state here, then call `immediate_refresh` to tell Zed to
    /// re-request semantic tokens right away. `immediate_refresh` also cancels any pending debounced refresh to avoid
    /// a redundant second re-request 250 ms later.
    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<serde_json::Value>> {
        match params.command.as_str() {
            "highlight.toggle" => {
                // The word was embedded as the first argument by `code_action`.
                let word = params
                    .arguments
                    .into_iter()
                    .next()
                    .and_then(|v| v.as_str().map(str::to_owned));
                if let Some(w) = word {
                    self.state.lock().await.toggle(&w);
                    self.immediate_refresh().await;
                }
            }
            "highlight.clear" => {
                self.state.lock().await.words.clear();
                self.immediate_refresh().await;
            }
            _ => {}
        }
        Ok(None)
    }
}

/// Entry point of the LSP server.
///
/// LSP servers communicate over stdin/stdout. tower-lsp handles the JSON-RPC framing and dispatches each message to
/// the appropriate handler on the Backend. The server runs until stdin is closed (i.e. until the editor exits or
/// restarts the language server).
#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}

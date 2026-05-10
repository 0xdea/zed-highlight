#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/0xdea/zed-highlight/master/.img/logo.png"
)]
#![allow(
    clippy::wildcard_imports,
    reason = "Implicit import of all LSP types is more convenient than listing them one by one."
)]

use std::collections::HashMap;
use std::option::Option;
use std::sync::Arc;
use std::time::Duration;

use regex::Regex;
use tokio::sync::Mutex;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

/// Package name.
const PROGRAM: &str = env!("CARGO_PKG_NAME");
/// Package version.
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// How many distinct highlight colors to cycle through.
const NUM_COLORS: usize = 8;
/// Delay in milliseconds for the debounced refresh after edits.
const DEBOUNCE_DELAY_MS: u64 = 250;

/// These names are arbitrary strings that the LSP advertises as its semantic token type legend. Zed looks them up in
/// `global_lsp_settings.semantic_token_rules` (settings.json file) to map each name to a foreground/background color.
///
/// ## Examples
///
/// `global_lsp_settings` snippet for the dark theme (8 highlight colors with 50% opacity backgrounds):
/// ```json
/// "global_lsp_settings": {
///   "semantic_token_rules": [
///     { "token_type": "zed-highlight-0", "foreground_color": "#F5B041", "background_color": "#F5B04150" },
///     { "token_type": "zed-highlight-1", "foreground_color": "#85C1E9", "background_color": "#85C1E950" },
///     { "token_type": "zed-highlight-2", "foreground_color": "#CD6155", "background_color": "#CD615550" },
///     { "token_type": "zed-highlight-3", "foreground_color": "#AF7AC5", "background_color": "#AF7AC550" },
///     { "token_type": "zed-highlight-4", "foreground_color": "#48C9B0", "background_color": "#48C9B050" },
///     { "token_type": "zed-highlight-5", "foreground_color": "#F4D03F", "background_color": "#F4D03F50" },
///     { "token_type": "zed-highlight-6", "foreground_color": "#52BE80", "background_color": "#52BE8050" },
///     { "token_type": "zed-highlight-7", "foreground_color": "#FF9933", "background_color": "#FF993350" },
///   ],
/// },
/// ```
static TOKEN_TYPE_NAMES: [&str; NUM_COLORS] = [
    "zed-highlight-0",
    "zed-highlight-1",
    "zed-highlight-2",
    "zed-highlight-3",
    "zed-highlight-4",
    "zed-highlight-5",
    "zed-highlight-6",
    "zed-highlight-7",
];

/// LSP server's internal state.
///
/// We track the list of highlighted words and the full text of every open document so we can scan for token positions
/// on demand. We also track the user's settings for whole-word and case-insensitive matching. All state is kept in
/// memory and shared across all documents and tabs, so highlighting a word in one file highlights it in all files.
struct State {
    /// The list of currently highlighted words. A removed slot becomes `None` and subsequent entries are not shifted.
    words: Vec<Option<String>>,

    /// Full text of every open document, keyed by URI.
    docs: HashMap<Url, String>,

    /// Whether to only match whole words (e.g., "for" doesn't match inside "format").
    whole_word: bool,

    /// Whether to ignore case when matching (e.g., "Foo" matches "foo").
    ignore_case: bool,
}

impl State {
    /// Construct a new instance of the server's internal state.
    ///
    /// By default, `whole_word` matching is set to true, while the `ignore_case` flag defaults to false. This should be
    /// a sensible default behavior, and we can consider making these flags configurable later if there's demand.
    fn new() -> Self {
        Self {
            words: Vec::new(),
            docs: HashMap::new(),
            whole_word: true,
            ignore_case: false,
        }
    }

    /// Toggle a word in/out of the highlight list.
    ///
    /// Three cases:
    /// 1. Word is already in the list: soft-delete it preserving the slot (set its slot to `None`).
    /// 2. Word is new and there's a `None` slot: reuse the first free slot so we reclaim the color index.
    /// 3. Word is new and there are no free slots: grow the list appending a new `Some(word)`.
    fn toggle(&mut self, word: &str) {
        if let Some(idx) = self
            .words
            .iter()
            .position(|o| o.as_deref().is_some_and(|w| self.words_eq(w, word)))
        {
            // Case 1: Word is already in the list: soft-delete it preserving the slot (set its slot to `None`).
            self.words[idx] = None;
        } else if let Some(slot) = self.words.iter_mut().find(|o| o.is_none()) {
            // Case 2: Word is new and there's a `None` slot: reuse the first free slot so we reclaim the color index.
            *slot = Some(word.to_owned());
        } else {
            // Case 3: Word is new and there are no free slots: grow the list appending a new `Some(word)`.
            self.words.push(Some(word.to_owned()));
        }
    }

    /// Check whether at least one word is currently highlighted.
    fn has_any(&self) -> bool {
        self.words.iter().any(Option::is_some)
    }

    /// Check whether a word is currently highlighted.
    fn is_highlighted(&self, word: &str) -> bool {
        self.words
            .iter()
            .any(|o| o.as_deref().is_some_and(|w| self.words_eq(w, word)))
    }

    /// Clear all highlighted words.
    fn words_clear(&mut self) {
        self.words.clear();
    }

    /// Helper function to compare two words for equality, respecting the `ignore_case` flag.
    fn words_eq(&self, a: &str, b: &str) -> bool {
        if self.ignore_case {
            a.to_lowercase() == b.to_lowercase()
        } else {
            a == b
        }
    }
}

/// LSP server's backend.
///
/// This struct holds the shared state and implements the [`LanguageServer`] trait that tower-lsp dispatches to. Each
/// method corresponds to a particular LSP request/notification that Zed sends us. The main logic is in
/// [`Backend::build_tokens`], which scans the document for matches and encodes the token positions in the format
/// required by the LSP semantic tokens protocol.
struct Backend {
    /// The tower-lsp client handle used to send requests (e.g., `workspace/semanticTokens/refresh`) to Zed.
    client: Client,

    /// Shared mutable state behind a tokio async `Mutex`.
    state: Arc<Mutex<State>>,

    /// Handle to the currently pending debounced refresh task, if any.
    refresh_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Backend {
    /// Construct a new instance of the server's backend given a [`Client`].
    fn new(client: Client) -> Self {
        Self {
            client,
            state: Arc::new(Mutex::new(State::new())),
            refresh_handle: Mutex::new(None),
        }
    }

    /// Cancel any pending debounced refresh and send a `workspace/semanticTokens/refresh` request to Zed right now.
    /// Used after user-driven actions (toggle/clear) where we want the highlight change to appear without delay.
    #[expect(
        clippy::let_underscore_must_use,
        reason = "Errors from the refresh request are intentionally ignored."
    )]
    async fn immediate_refresh(&self) {
        // Cancel any pending debounced refresh.
        let refresh_handle = self.refresh_handle.lock().await.take();
        if let Some(h) = refresh_handle {
            h.abort();
        }

        // Send the refresh request to Zed, ignoring any errors.
        let _ = self.client.semantic_tokens_refresh().await;
    }

    /// Schedule a `workspace/semanticTokens/refresh` request after a short idle delay, cancelling any previously
    /// scheduled one. Classic debounce pattern: rapid events (keystrokes) keep resetting the timer; the refresh only
    /// fires once the user pauses.
    #[expect(
        clippy::let_underscore_must_use,
        reason = "Errors from the refresh request are intentionally ignored."
    )]
    async fn debounced_refresh(&self) {
        let mut guard = self.refresh_handle.lock().await;

        // Cancel any pending debounced refresh.
        if let Some(h) = guard.take() {
            h.abort();
        }

        // Schedule a new debounced refresh request.
        let client = self.client.clone();
        *guard = Some(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(DEBOUNCE_DELAY_MS)).await;
            let _ = client.semantic_tokens_refresh().await;
        }));
    }

    /// Build the full list of [`SemanticTokens`] for a document.
    ///
    /// The LSP semantic tokens protocol requires tokens to be encoded as a flat array of [`SemanticToken`] 5-tuples in
    /// document order, where each position is expressed as a delta from the previous token (not an absolute position).
    /// This lets the client decode the stream in one pass without random access.
    ///
    /// Character offsets must be in UTF-16 code units because that is what the LSP spec mandates.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "UTF-16 code unit count should fit in u32 for any reasonable line length."
    )]
    async fn build_tokens(&self, uri: &Url) -> Vec<SemanticToken> {
        // Snapshot the state and release the lock.
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

        // Collect all matches as absolute (`line`, `start`, `length`, `token_type`) tuples.
        let mut raw: Vec<(u32, u32, u32, u32)> = Vec::new();

        for (color_idx, opt) in words.iter().enumerate() {
            let word = match opt {
                Some(w) if !w.is_empty() => w,
                // Skip `None` (soft-deleted) and empty-string slots.
                _ => continue,
            };

            // Compile the regex for this word.
            let Some(re) = compile_word_regex(word, whole_word, ignore_case) else {
                // If the regex fails to compile, skip this word.
                continue;
            };

            // Color index wraps around if more than `NUM_COLORS` words are highlighted simultaneously.
            let token_type = (color_idx % NUM_COLORS) as u32;

            for (line_idx, line) in content.lines().enumerate() {
                for m in re.find_iter(line) {
                    // The LSP protocol requires UTF-16 character offsets, so we convert.
                    let start = utf16_len(&line[..m.start()]);
                    let length = utf16_len(m.as_str());
                    raw.push((line_idx as u32, start, length, token_type));
                }
            }
        }

        // Sort by (`line`, `start`).
        raw.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

        // Convert absolute positions to the LSP delta encoding.
        //
        // Rules from the spec:
        // delta_line  = this_line - prev_line
        // delta_start = this_start - prev_start   (only when delta_line == 0)
        //             = this_start                (when delta_line > 0)
        // i.e., the start offset resets to absolute whenever the line changes.
        let mut tokens = Vec::with_capacity(raw.len());
        let mut prev_line = 0;
        let mut prev_start = 0;

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
                token_modifiers_bitset: 0, // No modifiers.
            });
            prev_line = line;
            prev_start = start;
        }

        tokens
    }
}

/// LSP server's implementation.
///
/// We implement the [`LanguageServer`] trait from tower-lsp, which requires to define an async method for each LSP
/// request/notification we want to handle. The [`Backend`] struct holds the shared state and client handle, and we
/// dispatch to helper methods for the main logic.
#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    /// Called once at server startup. We respond with our capabilities so Zed knows which features we support.
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

                // Code actions appear in the "editor: toggle code actions" menu (accessed with the `⌘.` shortcut or the
                // lightning bolt icon in the gutter). We use them to surface "Highlight: <word>", "Remove highlight:
                // <word>", and "Clear all highlights" actions without requiring the user to bind a custom keymap entry.
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),

                // Register each supported command name so Zed knows to route `executeCommand` calls to this server.
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![
                        "zed-highlight.toggle".to_owned(),
                        "zed-highlight.clear".to_owned(),
                    ],
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                }),

                ..Default::default()
            },

            // Provide optional LSP server information.
            server_info: Some(ServerInfo {
                name: PROGRAM.to_owned(),
                version: Some(VERSION.to_owned()),
            }),
        })
    }

    /// Called after [`Backend::initialize`], once the client is ready to receive requests. Empty in our simple server.
    async fn initialized(&self, _: InitializedParams) {}

    /// Called when the server is shutting down. We have no resources to clean up in our simple server.
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    /// Called when Zed opens a document for the first time (not on every tab switch to an already-open file).
    ///
    /// To prevent race conditions, after storing the document we schedule a debounced refresh, which asks Zed to
    /// re-request tokens [`DEBOUNCE_DELAY_MS`] later, by which time `state.docs` is guaranteed to be up to date.
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        // Store the document text in `state.docs`.
        let has_any = {
            let mut state = self.state.lock().await;
            state
                .docs
                .insert(params.text_document.uri, params.text_document.text);
            state.has_any()
        };

        // Schedule a debounced refresh to update the tokens if there are highlighted words.
        if has_any {
            self.debounced_refresh().await;
        }
    }

    /// Called on every document edit.
    ///
    /// [`Backend::debounced_refresh`] is the safety net that corrects any stale token response once typing pauses.
    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // Update the document text in `state.docs`.
        let has_any = {
            let mut state = self.state.lock().await;
            // With FULL sync, there should always be exactly one content change with the full new text, but we
            // defensively handle the case where it's missing just in case.
            if let Some(change) = params.content_changes.into_iter().last() {
                state.docs.insert(params.text_document.uri, change.text);
            }
            state.has_any()
        };

        // Schedule a debounced refresh to update the tokens if there are highlighted words.
        if has_any {
            self.debounced_refresh().await;
        }
    }

    /// Called when a document is closed.
    ///
    /// We evict the document to reclaim memory; if the file is reopened, `did_open` will re-populate `state.docs`.
    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.state
            .lock()
            .await
            .docs
            .remove(&params.text_document.uri);
    }

    /// Called whenever Zed wants the current highlight tokens for a document, which happens:
    /// - When the document first opens.
    /// - After each [`Backend::did_change`] (Zed's own auto-request).
    /// - In response to our `workspace/semanticTokens/refresh` request.
    ///
    /// Returns `result_id: None`, meaning delta tokens are not supported.
    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        // Build the token list for the requested document.
        let data = self.build_tokens(&params.text_document.uri).await;
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        })))
    }

    /// Called whenever Zed opens the "editor: toggle code actions" menu.
    ///
    /// We return up to two actions:
    /// - "Highlight: <word>" or "Remove highlight: <word>" (if cursor is on a word or selection).
    /// - "Clear all highlights" (only if there are any active highlights).
    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        // Snapshot the state.
        let state = self.state.lock().await;
        let content = match state.docs.get(&params.text_document.uri) {
            Some(c) => c.clone(),
            None => return Ok(None),
        };
        let has_any = state.has_any();

        // Find the highlightable word the user is acting on, if any, and determine whether it's already highlighted.
        let word = word_at(&content, params.range)
            .filter(|w| is_highlightable(w, state.whole_word))
            .filter(|w| matches_anywhere(&content, w, state.whole_word, state.ignore_case));
        let already_highlighted = word.as_deref().is_some_and(|w| state.is_highlighted(w));

        // Explicitly release the lock before building the response.
        drop(state);

        // Build the list of code actions to return.
        let mut actions: Vec<CodeActionOrCommand> = Vec::new();

        // Highlight toggle action for the current word, if any.
        if let Some(ref w) = word {
            let title = if already_highlighted {
                format!("Remove highlight: \"{w}\"")
            } else {
                format!("Highlight: \"{w}\"")
            };
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::EMPTY),
                // The [`Command`] is embedded in the [`CodeAction`] and passed back to [`Backend::execute_command]`
                // when the user selects this item. We encode the word as the single argument.
                command: Some(Command {
                    title: "Toggle Highlight".to_owned(),
                    command: "zed-highlight.toggle".to_owned(),
                    arguments: Some(vec![serde_json::Value::String(w.clone())]),
                }),
                ..Default::default()
            }));
        }

        // Clear all highlights action, only if there are any active highlights.
        if has_any {
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: "Clear all highlights".to_owned(),
                kind: Some(CodeActionKind::EMPTY),
                command: Some(Command {
                    title: "Clear All Highlights".to_owned(),
                    command: "zed-highlight.clear".to_owned(),
                    arguments: None,
                }),
                ..Default::default()
            }));
        }

        Ok(Some(actions))
    }

    /// Called when the user selects a code action in the "editor: toggle code actions" menu.
    ///
    /// We mutate state, then call [`Backend::immediate_refresh`] to tell Zed to re-request semantic tokens right away.
    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<serde_json::Value>> {
        match params.command.as_str() {
            // Toggle the highlight of the word embedded in the command arguments.
            "zed-highlight.toggle" => {
                // The word was embedded as the first argument by [`Backend::code_action`].
                let word = params
                    .arguments
                    .into_iter()
                    .next()
                    .and_then(|v| v.as_str().map(str::to_owned));
                if let Some(w) = word {
                    // Toggle the word in the highlight list only if it's highlightable.
                    let toggled = {
                        let mut state = self.state.lock().await;
                        if is_highlightable(&w, state.whole_word) {
                            state.toggle(&w);
                            true
                        } else {
                            false
                        }
                    };
                    if toggled {
                        self.immediate_refresh().await;
                    }
                }
            }

            // Clear all highlights by clearing the list of highlighted words.
            "zed-highlight.clear" => {
                self.state.lock().await.words_clear();
                self.immediate_refresh().await;
            }

            // Silently ignore unknown commands.
            _ => {}
        }

        // No meaningful result to return.
        Ok(None)
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
    let mut count = 0;
    for (i, c) in s.char_indices() {
        // Check the offset before counting the current character.
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

/// Helper function to return the word the user is acting on, given the cursor range from a code action request.
///
/// Two cases:
/// 1. Non-empty single-line selection: use the selected text directly.
/// 2. Cursor (empty range, or multi-line): find the word under the cursor by scanning backwards and forwards.
///
/// "Word characters" are alphanumerics plus underscore, matching `\w` in most regex flavors and covering the common
/// case of identifiers in source code.
fn word_at(content: &str, range: Range) -> Option<String> {
    // Get the line where the cursor is. If the line is missing (shouldn't happen with valid LSP data), return None.
    let line = content.lines().nth(range.start.line as usize)?;

    // Case 1: Use the non-empty single-line selection directly (multi-line selections fall through to case 2).
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

    // If the cursor is on a non-word character, there's nothing to highlight (cursor is on a space or punctuation).
    if !line[byte_pos..].chars().next().is_some_and(is_word_char) {
        return None;
    }

    // Scan left from the cursor position to find the start of the word.
    let start = line[..byte_pos]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word_char(*c))
        .last()
        .map_or(byte_pos, |(i, _)| i);

    // Scan right from the cursor position to find the end of the word.
    let end = byte_pos
        + line[byte_pos..]
            .char_indices()
            .take_while(|(_, c)| is_word_char(*c))
            .last()
            .map_or(0, |(i, c)| i + c.len_utf8());

    if start < end {
        Some(line[start..end].to_string())
    } else {
        None
    }
}

/// Helper function to check if a character is a "word character" for the purposes of determining word boundaries.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Helper function to check whether a given text can produce visible highlights based on the current matching rules.
///
/// In `whole_word` mode the pattern `\b<escaped>\b` only matches when the first and last characters of the candidate
/// are word characters. Candidates failing this rule would compile into a regex that never matches, so we use this
/// predicate to filter them out early at the code action layer rather than letting them sit in `words` invisibly.
fn is_highlightable(text: &str, whole_word: bool) -> bool {
    if text.is_empty() {
        return false;
    }
    if !whole_word {
        return true;
    }

    let starts_ok = text.chars().next().is_some_and(is_word_char);
    let ends_ok = text.chars().next_back().is_some_and(is_word_char);

    starts_ok && ends_ok
}

/// Helper function to check whether a given text produces at least one visible highlight in the current document under
/// the current matching rules. We do not match in all open documents.
///
/// This is the strongest predicate we can use to decide if a code action menu entry is worth showing: it asks the
/// exact question whose answer determines whether the user would see anything change after toggling, at a somewhat
/// negligible performance cost.
fn matches_anywhere(content: &str, text: &str, whole_word: bool, ignore_case: bool) -> bool {
    let Some(re) = compile_word_regex(text, whole_word, ignore_case) else {
        return false;
    };
    content.lines().any(|line| re.is_match(line))
}

/// Helper function to compile a regex for a given word, escaping it first so that punctuation is treated literally, and
/// respecting the `whole_word` and `ignore_case` flags.
///
/// Returns `None` if the pattern fails to compile (unlikely for an escaped literal).
fn compile_word_regex(word: &str, whole_word: bool, ignore_case: bool) -> Option<Regex> {
    // Treat the word as a literal string by escaping it.
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
    Regex::new(&pattern).ok()
}

/// Entry point of the LSP server.
///
/// LSP servers communicate over `stdin`/`stdout`; tower-lsp handles the JSON-RPC framing and dispatches each message to
/// the appropriate handler on the [`Backend`]. The server runs until `stdin` is closed (i.e., until the editor exits or
/// restarts the language server).
#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}

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
    /// By default, [`State::whole_word`] matching is set to true, while the [`State::ignore_case`] flag defaults to
    /// false. This should be a sensible default behavior, and we can consider making these flags configurable later.
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

    /// Helper function to compare two words for equality, respecting the [`State::ignore_case`] flag.
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
                // lightning bolt icon in the gutter). We use them to surface `Highlight: <word>`, `Remove highlight:
                // <word>`, and `Clear all highlights` actions without requiring the user to bind a custom keymap entry.
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
    /// - `Highlight: <word>` or `Remove highlight: <word>` (if cursor is on a word or selection).
    /// - `Clear all highlights` (only if there are any active highlights).
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
/// respecting the [`State::whole_word`] and [`State::ignore_case`] flags.
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

#[expect(clippy::unwrap_used, reason = "tests can use `unwrap`")]
#[cfg(test)]
mod tests {
    use super::*;

    // Helper functions.

    fn make_range(sl: u32, sc: u32, el: u32, ec: u32) -> Range {
        Range {
            start: Position {
                line: sl,
                character: sc,
            },
            end: Position {
                line: el,
                character: ec,
            },
        }
    }

    fn cursor_range(line: u32, character: u32) -> Range {
        make_range(line, character, line, character)
    }

    // Test `State::new`.

    #[test]
    fn state_new_has_empty_word_list() {
        let s = State::new();
        assert!(s.words.is_empty());
    }

    #[test]
    fn state_new_has_empty_docs() {
        let s = State::new();
        assert!(s.docs.is_empty());
    }

    #[test]
    fn state_new_defaults_whole_word_true() {
        let s = State::new();
        assert!(s.whole_word);
    }

    #[test]
    fn state_new_defaults_ignore_case_false() {
        let s = State::new();
        assert!(!s.ignore_case);
    }

    // Test `State::toggle`.

    #[test]
    fn state_toggle_adds_new_word() {
        let mut s = State::new();
        s.toggle("foo");
        assert_eq!(s.words, vec![Some("foo".to_owned())]);
    }

    #[test]
    fn state_toggle_removes_existing_word_leaving_none_slot() {
        let mut s = State::new();
        s.toggle("foo");
        s.toggle("foo");
        assert_eq!(s.words, vec![None]);
    }

    #[test]
    fn state_toggle_reuses_first_none_slot_for_new_word() {
        let mut s = State::new();
        s.toggle("foo"); // slot 0 = Some("foo")
        s.toggle("foo"); // slot 0 = None
        s.toggle("bar"); // should reuse slot 0, not grow
        assert_eq!(s.words, vec![Some("bar".to_owned())]);
    }

    #[test]
    fn state_toggle_grows_list_when_no_free_slots() {
        let mut s = State::new();
        s.toggle("a");
        s.toggle("b");
        assert_eq!(s.words.len(), 2);
        assert!(s.words.iter().all(Option::is_some));
    }

    #[test]
    fn state_toggle_leaves_other_words_in_place_after_removal() {
        let mut s = State::new();
        s.toggle("a");
        s.toggle("b");
        s.toggle("a"); // soft-delete "a"
        assert!(s.words[0].is_none(), "removed slot must be `None`");
        assert_eq!(s.words[1], Some("b".to_owned()));
    }

    #[test]
    fn state_toggle_respects_ignore_case_for_deduplication() {
        let mut s = State::new();
        s.ignore_case = true;
        s.toggle("Foo");
        s.toggle("foo"); // should match "Foo" and remove it
        assert!(!s.is_highlighted("foo"));
        assert!(!s.is_highlighted("Foo"));
    }

    // Test `State::has_any`.

    #[test]
    fn state_has_any_false_when_empty() {
        assert!(!State::new().has_any());
    }

    #[test]
    fn state_has_any_false_when_all_slots_are_none() {
        let mut s = State::new();
        s.toggle("a");
        s.toggle("a");
        assert!(!s.has_any());
    }

    #[test]
    fn state_has_any_true_when_at_least_one_word_present() {
        let mut s = State::new();
        s.toggle("a");
        assert!(s.has_any());
    }

    #[test]
    fn state_has_any_true_with_mixed_none_and_some() {
        let mut s = State::new();
        s.toggle("a");
        s.toggle("b");
        s.toggle("a"); // remove "a", keep "b"
        assert!(s.has_any());
    }

    // Test `State::is_highlighted`.

    #[test]
    fn state_is_highlighted_true_for_present_word() {
        let mut s = State::new();
        s.toggle("foo");
        assert!(s.is_highlighted("foo"));
    }

    #[test]
    fn state_is_highlighted_false_for_absent_word() {
        let mut s = State::new();
        s.toggle("foo");
        assert!(!s.is_highlighted("bar"));
    }

    #[test]
    fn state_is_highlighted_false_after_removal() {
        let mut s = State::new();
        s.toggle("foo");
        s.toggle("foo");
        assert!(!s.is_highlighted("foo"));
    }

    // Test `State::words_clear`.

    #[test]
    fn state_words_clear_empties_list() {
        let mut s = State::new();
        s.toggle("a");
        s.toggle("b");
        s.words_clear();
        assert!(s.words.is_empty());
    }

    #[test]
    fn state_words_clear_results_in_has_any_false() {
        let mut s = State::new();
        s.toggle("a");
        s.words_clear();
        assert!(!s.has_any());
    }

    // Test `State::words_eq`.

    #[test]
    fn state_words_eq_identical_strings() {
        let s = State::new();
        assert!(s.words_eq("hello", "hello"));
    }

    #[test]
    fn state_words_eq_case_sensitive_by_default() {
        let s = State::new();
        assert!(!s.words_eq("Foo", "foo"));
    }

    #[test]
    fn state_words_eq_case_insensitive_when_flag_set() {
        let mut s = State::new();
        s.ignore_case = true;
        assert!(s.words_eq("Foo", "foo"));
        assert!(s.words_eq("FOO", "foo"));
    }

    #[test]
    fn state_words_eq_different_words_always_false() {
        let mut s = State::new();
        assert!(!s.words_eq("foo", "bar"));
        s.ignore_case = true;
        assert!(!s.words_eq("foo", "bar"));
    }

    // Test `utf16_len`.

    #[test]
    fn utf16_len_empty_string_is_zero() {
        assert_eq!(utf16_len(""), 0);
    }

    #[test]
    fn utf16_len_ascii_counts_one_per_char() {
        assert_eq!(utf16_len("hello"), 5);
    }

    #[test]
    fn utf16_len_bmp_multibyte_char_counts_one() {
        // '£' is U+00A3: 2 UTF-8 bytes, 1 UTF-16 code unit.
        assert_eq!(utf16_len("£"), 1);
        // '中' is U+4E2D: 3 UTF-8 bytes, 1 UTF-16 code unit.
        assert_eq!(utf16_len("中文"), 2);
    }

    #[test]
    fn utf16_len_supplementary_char_counts_two() {
        // '😀' is U+1F600: 4 UTF-8 bytes, 2 UTF-16 code units (surrogate pair).
        assert_eq!(utf16_len("😀"), 2);
    }

    #[test]
    fn utf16_len_mixed_content() {
        // "a😀b" = 1 + 2 + 1 = 4 UTF-16 code units.
        assert_eq!(utf16_len("a😀b"), 4);
    }

    // Test `utf16_to_byte`.

    #[test]
    fn utf16_to_byte_offset_zero_is_string_start() {
        assert_eq!(utf16_to_byte("hello", 0), Some(0));
    }

    #[test]
    fn utf16_to_byte_ascii_offsets_equal_byte_offsets() {
        assert_eq!(utf16_to_byte("hello", 3), Some(3));
    }

    #[test]
    fn utf16_to_byte_offset_at_end_of_string() {
        assert_eq!(utf16_to_byte("hello", 5), Some(5));
    }

    #[test]
    fn utf16_to_byte_offset_past_end_returns_none() {
        assert_eq!(utf16_to_byte("hello", 6), None);
    }

    #[test]
    fn utf16_to_byte_bmp_multibyte_char() {
        // "£a": '£' occupies 1 UTF-16 unit but 2 UTF-8 bytes.
        // UTF-16 offset 1 -> byte offset 2.
        assert_eq!(utf16_to_byte("£a", 1), Some(2));
        // UTF-16 offset 2 -> byte offset 3 (end of string).
        assert_eq!(utf16_to_byte("£a", 2), Some(3));
    }

    #[test]
    fn utf16_to_byte_surrogate_pair() {
        // "😀a": '😀' occupies 2 UTF-16 units but 4 UTF-8 bytes.
        // UTF-16 offset 2 -> byte offset 4.
        assert_eq!(utf16_to_byte("😀a", 2), Some(4));
    }

    #[test]
    fn utf16_to_byte_empty_string_offset_zero() {
        assert_eq!(utf16_to_byte("", 0), Some(0));
    }

    #[test]
    fn utf16_to_byte_empty_string_nonzero_is_none() {
        assert_eq!(utf16_to_byte("", 1), None);
    }

    // Test `is_word_char`.

    #[test]
    fn is_word_char_ascii_letters() {
        assert!(is_word_char('a'));
        assert!(is_word_char('z'));
        assert!(is_word_char('A'));
        assert!(is_word_char('Z'));
    }

    #[test]
    fn is_word_char_digits() {
        assert!(is_word_char('0'));
        assert!(is_word_char('9'));
    }

    #[test]
    fn is_word_char_underscore() {
        assert!(is_word_char('_'));
    }

    #[test]
    fn is_word_char_space_is_false() {
        assert!(!is_word_char(' '));
        assert!(!is_word_char('\t'));
        assert!(!is_word_char('\n'));
    }

    #[test]
    fn is_word_char_punctuation_is_false() {
        for c in [
            '.', ',', '!', '(', ')', '-', '+', '=', '*', '/', '\\', '"', '\'', ';', ':',
        ] {
            assert!(!is_word_char(c), "'{c}' should not be a word char");
        }
    }

    // Test `is_highlightable`.

    #[test]
    fn is_highlightable_empty_string_always_false() {
        assert!(!is_highlightable("", false));
        assert!(!is_highlightable("", true));
    }

    #[test]
    fn is_highlightable_any_nonempty_without_whole_word() {
        assert!(is_highlightable("foo", false));
        assert!(is_highlightable("(bar)", false));
        assert!(is_highlightable("foo bar", false));
    }

    #[test]
    fn is_highlightable_identifier_in_whole_word_mode() {
        assert!(is_highlightable("foo", true));
        assert!(is_highlightable("foo_bar", true));
        assert!(is_highlightable("foo123", true));
        assert!(is_highlightable("_private", true));
    }

    #[test]
    fn is_highlightable_leading_nonword_char_fails_whole_word() {
        assert!(!is_highlightable("(foo", true));
        assert!(!is_highlightable(".foo", true));
        assert!(!is_highlightable(" foo", true));
    }

    #[test]
    fn is_highlightable_trailing_nonword_char_fails_whole_word() {
        assert!(!is_highlightable("foo(", true));
        assert!(!is_highlightable("foo.", true));
        assert!(!is_highlightable("foo ", true));
    }

    #[test]
    fn is_highlightable_single_word_char_is_valid() {
        assert!(is_highlightable("x", true));
        assert!(is_highlightable("_", true));
        assert!(is_highlightable("1", true));
    }

    // Test `compile_word_regex`.

    #[test]
    fn compile_word_regex_basic_match() {
        let re = compile_word_regex("foo", false, false).unwrap();
        assert!(re.is_match("foo"));
    }

    #[test]
    fn compile_word_regex_matches_substring_without_whole_word() {
        let re = compile_word_regex("for", false, false).unwrap();
        assert!(re.is_match("format"));
    }

    #[test]
    fn compile_word_regex_whole_word_rejects_substring() {
        let re = compile_word_regex("for", true, false).unwrap();
        assert!(
            !re.is_match("format"),
            "whole-word 'for' must not match inside 'format'"
        );
        assert!(
            !re.is_match("therefore"),
            "whole-word 'for' must not match inside 'therefore'"
        );
    }

    #[test]
    fn compile_word_regex_whole_word_matches_standalone() {
        let re = compile_word_regex("for", true, false).unwrap();
        assert!(re.is_match("for x in y"));
        assert!(re.is_match("(for)"));
    }

    #[test]
    fn compile_word_regex_case_sensitive_by_default() {
        let re = compile_word_regex("Foo", false, false).unwrap();
        assert!(re.is_match("Foo"));
        assert!(!re.is_match("foo"));
        assert!(!re.is_match("FOO"));
    }

    #[test]
    fn compile_word_regex_case_insensitive_flag() {
        let re = compile_word_regex("Foo", false, true).unwrap();
        assert!(re.is_match("Foo"));
        assert!(re.is_match("foo"));
        assert!(re.is_match("FOO"));
    }

    #[test]
    fn compile_word_regex_escapes_dot_as_literal() {
        let re = compile_word_regex("foo.bar", false, false).unwrap();
        assert!(re.is_match("foo.bar"));
        assert!(
            !re.is_match("fooXbar"),
            "dot must match literally, not as any-char"
        );
    }

    #[test]
    fn compile_word_regex_escapes_parentheses() {
        let re = compile_word_regex("foo()", false, false).unwrap();
        assert!(re.is_match("foo()"));
        assert!(!re.is_match("foo"), "parentheses are not optional");
    }

    #[test]
    fn compile_word_regex_escapes_star() {
        let re = compile_word_regex("a*b", false, false).unwrap();
        assert!(re.is_match("a*b"));
        assert!(!re.is_match("ab"), "star must be literal, not a quantifier");
    }

    // Test `matches_anywhere`.

    #[test]
    fn matches_anywhere_finds_word_in_content() {
        assert!(matches_anywhere("let foo = 1;", "foo", true, false));
    }

    #[test]
    fn matches_anywhere_returns_false_for_absent_word() {
        assert!(!matches_anywhere("let foo = 1;", "bar", true, false));
    }

    #[test]
    fn matches_anywhere_whole_word_rejects_substring() {
        assert!(!matches_anywhere("format!()", "for", true, false));
    }

    #[test]
    fn matches_anywhere_non_whole_word_finds_substring() {
        assert!(matches_anywhere("format!()", "for", false, false));
    }

    #[test]
    fn matches_anywhere_case_insensitive_finds_match() {
        assert!(matches_anywhere("let Foo = 1;", "foo", false, true));
    }

    #[test]
    fn matches_anywhere_multiline_content_any_line() {
        let content = "line one\nfoo here\nline three";
        assert!(matches_anywhere(content, "foo", true, false));
    }

    #[test]
    fn matches_anywhere_empty_content_returns_false() {
        assert!(!matches_anywhere("", "foo", true, false));
    }

    #[test]
    fn matches_anywhere_word_not_on_this_line_returns_false() {
        assert!(!matches_anywhere(
            "line one\nline two",
            "three",
            true,
            false
        ));
    }

    // Test `word_at`.

    #[test]
    fn word_at_selection_returns_selected_text() {
        // "let foo = 1;" - select "foo" at UTF-16 chars 4..7.
        let range = make_range(0, 4, 0, 7);
        assert_eq!(word_at("let foo = 1;", range), Some("foo".to_owned()));
    }

    #[test]
    fn word_at_selection_trims_surrounding_whitespace() {
        // "let foo = 1;" - select " foo " at chars 3..8.
        let range = make_range(0, 3, 0, 8);
        assert_eq!(word_at("let foo = 1;", range), Some("foo".to_owned()));
    }

    #[test]
    fn word_at_cursor_in_middle_of_word() {
        // "hello world" - cursor on 'o' (char 4) -> word "hello".
        assert_eq!(
            word_at("hello world", cursor_range(0, 4)),
            Some("hello".to_owned())
        );
    }

    #[test]
    fn word_at_cursor_at_start_of_word() {
        assert_eq!(
            word_at("hello world", cursor_range(0, 0)),
            Some("hello".to_owned())
        );
    }

    #[test]
    fn word_at_cursor_just_past_word_end_is_none() {
        // char 5 in "hello world" is the space between the words.
        assert_eq!(word_at("hello world", cursor_range(0, 5)), None);
    }

    #[test]
    fn word_at_cursor_on_punctuation_is_none() {
        // "foo(bar)" - char 3 is '('.
        assert_eq!(word_at("foo(bar)", cursor_range(0, 3)), None);
    }

    #[test]
    fn word_at_cursor_on_second_line() {
        let content = "first\nsecond line";
        // line 1, char 0 -> 's' in "second".
        assert_eq!(
            word_at(content, cursor_range(1, 0)),
            Some("second".to_owned())
        );
    }

    #[test]
    fn word_at_nonexistent_line_is_none() {
        assert_eq!(word_at("one line only", cursor_range(99, 0)), None);
    }

    #[test]
    fn word_at_word_with_underscores() {
        // "some_var = 1;" - cursor on 'v' (char 5).
        assert_eq!(
            word_at("some_var = 1;", cursor_range(0, 5)),
            Some("some_var".to_owned())
        );
    }

    #[test]
    fn word_at_multiline_selection_uses_start_position_for_cursor_word() {
        // Multi-line selection falls through to case 2 using range.start.
        // Line 0 char 4 is 'f' in "foo".
        let content = "let foo = 1;\nbar baz";
        let range = make_range(0, 4, 1, 3);
        assert_eq!(word_at(content, range), Some("foo".to_owned()));
    }

    #[test]
    fn word_at_selection_with_bmp_multibyte_chars() {
        // "中文 hello" - '中' and '文' are each 1 UTF-16 unit (3 UTF-8 bytes).
        // "hello" starts at UTF-16 offset 3, ends at offset 8.
        let range = make_range(0, 3, 0, 8);
        assert_eq!(word_at("中文 hello", range), Some("hello".to_owned()));
    }

    #[test]
    fn word_at_cursor_after_surrogate_pair() {
        // "😀foo" - emoji is 2 UTF-16 units; 'f' starts at UTF-16 offset 2.
        assert_eq!(word_at("😀foo", cursor_range(0, 2)), Some("foo".to_owned()));
    }

    #[test]
    fn word_at_empty_selection_text_after_trim_is_none() {
        // Selecting only whitespace (e.g., a space) should yield None.
        let range = make_range(0, 3, 0, 4); // the space in "foo bar"
        assert_eq!(word_at("foo bar", range), None);
    }
}

#![doc = env!("CARGO_PKG_DESCRIPTION")]
#![doc = ""]
#![cfg_attr(doc, doc = include_str!("../README.md"))]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/0xdea/zed-highlight/master/.img/logo.png"
)]
#![allow(
    clippy::wildcard_imports,
    reason = "implicit import of all LSP types is more convenient than listing them one by one"
)]

use std::collections::HashMap;
use std::option::Option;
use std::sync::Arc;
use std::time::Duration;

use regex::{Regex, RegexBuilder};
use tokio::sync::Mutex;
use tokio::{io, task, time};
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
/// # Examples
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

    /// Full text of every open document, keyed by URI. Stored behind `Arc` so handlers ([`Backend::build_tokens`] and
    /// [`Backend::code_action`]) can take a cheap clone under the state lock and release it before scanning, instead of
    /// duplicating the entire document on every request.
    docs: HashMap<Url, Arc<str>>,

    /// Whether to only match whole words (e.g., "for" doesn't match inside "format").
    whole_word: bool,

    /// Whether to ignore case when matching (e.g., "Foo" matches "foo").
    ignore_case: bool,
}

impl State {
    /// Constructs a new instance of the server's internal state.
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

    /// Toggles a word in/out of the highlight list.
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

    /// Checks whether at least one word is currently highlighted.
    fn has_any(&self) -> bool {
        self.words.iter().any(Option::is_some)
    }

    /// Clears all highlighted words.
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
    refresh_handle: Mutex<Option<task::JoinHandle<()>>>,
}

impl Backend {
    /// Constructs a new instance of the server's backend given a [`Client`].
    fn new(client: Client) -> Self {
        Self {
            client,
            state: Arc::new(Mutex::new(State::new())),
            refresh_handle: Mutex::new(None),
        }
    }

    /// Cancels any pending debounced refresh and sends a `workspace/semanticTokens/refresh` request to Zed right now.
    /// Used after user-driven actions (toggle/clear) where we want the highlight change to appear without delay.
    ///
    /// Zed does not implement `workspace/codeAction/refresh`, so it cannot be signalled to re-fetch code actions
    /// after a state change. The code action titles are therefore kept stateless (see [`Backend::code_action`])
    /// so that Zed's cached response remains accurate regardless of the direction of the last toggle.
    async fn immediate_refresh(&self) {
        // Cancel any pending debounced refresh.
        let refresh_handle = self.refresh_handle.lock().await.take();
        if let Some(h) = refresh_handle {
            h.abort();
        }

        // Send the refresh request to Zed, ignoring any errors.
        _ = self.client.semantic_tokens_refresh().await;
    }

    /// Schedules a `workspace/semanticTokens/refresh` request after a short idle delay, cancelling any previously
    /// scheduled one. Classic debounce pattern: rapid events (keystrokes) keep resetting the timer; the refresh only
    /// fires once the user pauses.
    async fn debounced_refresh(&self) {
        let mut guard = self.refresh_handle.lock().await;

        // Cancel any pending debounced refresh.
        if let Some(h) = guard.take() {
            h.abort();
        }

        // Schedule a new debounced refresh request.
        let client = self.client.clone();
        *guard = Some(tokio::spawn(async move {
            time::sleep(Duration::from_millis(DEBOUNCE_DELAY_MS)).await;
            _ = client.semantic_tokens_refresh().await;
        }));
    }

    /// Builds the full list of [`SemanticTokens`] for a document.
    ///
    /// The LSP semantic tokens protocol requires tokens to be encoded as a flat array of [`SemanticToken`] 5-tuples in
    /// document order, where each position is expressed as a delta from the previous token (not an absolute position).
    /// This lets the client decode the stream in one pass without random access.
    ///
    /// Character offsets must be in UTF-16 code units because that is what the LSP spec mandates.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "UTF-16 code unit count should fit in u32 for any reasonable line length"
    )]
    #[expect(
        clippy::integer_division_remainder_used,
        reason = "the modulo operation is needed here"
    )]
    #[expect(
        clippy::as_conversions,
        reason = "the `as` conversion is safe here because of the modulo operation"
    )]
    #[expect(
        clippy::arithmetic_side_effects,
        reason = "subtractions should be safe here"
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

        // Collect all matches as absolute (`line`, `start`, `length`, `token_type`) 4-tuples.
        let mut raw: Vec<(u32, u32, u32, u32)> = Vec::new();

        for (color_idx, opt) in words.iter().enumerate() {
            let word = match opt.as_deref() {
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
#[expect(
    clippy::missing_trait_methods,
    reason = "we need only a subset of the trait methods"
)]
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

                // Code actions appear in the "editor: toggle code actions" menu (accessed with the `⌘.`/`Ctrl+.`
                // shortcut or the lightning bolt icon in the gutter). We use them to surface the `Toggle highlight:
                // <word>` and `Clear all highlights` actions.
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
                .insert(params.text_document.uri, params.text_document.text.into());
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
                state
                    .docs
                    .insert(params.text_document.uri, change.text.into());
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
    /// - `Toggle highlight: <word>` (if cursor is on a valid word or selection).
    /// - `Clear all highlights` (only if there are any active highlights).
    ///
    /// The toggle action deliberately uses a stateless title ("Toggle highlight") rather than a state-dependent one
    /// ("Highlight" vs "Remove highlight"). Zed caches code action responses by cursor position and only invalidates
    /// that cache on cursor movement or document edits. A stateless title is therefore always accurate regardless of
    /// when Zed last fetched the response, and avoids the confusing mismatch of seeing "Highlight: foo" when the word
    /// is already highlighted (or vice versa) without the user having moved their cursor.
    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        // Snapshot the state.
        let state = self.state.lock().await;
        let content = match state.docs.get(&params.text_document.uri) {
            Some(c) => Arc::clone(c),
            None => return Ok(None),
        };
        let has_any = state.has_any();

        // Find the highlightable word the user is acting on, if any.
        let word = word_at(&content, params.range)
            .filter(|w| is_highlightable(w, state.whole_word))
            .filter(|w| matches_anywhere(&content, w, state.whole_word, state.ignore_case));

        // Explicitly release the lock before building the response.
        drop(state);

        // Build the list of code actions to return.
        let mut actions: Vec<CodeActionOrCommand> = Vec::new();

        // Highlight toggle action for the current word, if any.
        if let Some(w) = word.as_ref() {
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: format!("Toggle highlight: \"{w}\""),
                kind: Some(CodeActionKind::EMPTY),
                // The [`Command`] is embedded in the [`CodeAction`] and passed back to [`Backend::execute_command`]
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
    reason = "UTF-16 code unit count should fit in u32 for any reasonable line length"
)]
#[expect(
    clippy::as_conversions,
    reason = "the `as` conversion is reasonably safe here because we are operating on strings"
)]
fn utf16_len(s: &str) -> u32 {
    s.chars().map(|c| c.len_utf16() as u32).sum()
}

/// Helper function to convert a UTF-16 character offset to a byte offset within `s`.
#[expect(
    clippy::arithmetic_side_effects,
    reason = "`count` cannot reasonably overflow here"
)]
fn utf16_to_byte(s: &str, utf16_offset: usize) -> Option<usize> {
    let mut count = 0;
    for (i, c) in s.char_indices() {
        // Check the offset before counting the current character.
        if count == utf16_offset {
            return Some(i);
        }
        count += c.len_utf16();
    }
    // Returns `None` if the offset is past the end of the string (shouldn't happen with valid LSP data).
    (count == utf16_offset).then_some(s.len())
}

/// Helper function to return the word the user is acting on, given the cursor range from a code action request.
///
/// Two cases:
/// 1. Non-empty single-line selection: use the selected text directly.
/// 2. Cursor (empty range, or multi-line): find the word under the cursor by scanning backwards and forwards.
///
/// "Word characters" are alphanumerics plus underscore, matching `\w` in most regex flavors and covering the common
/// case of identifiers in source code.
#[expect(
    clippy::as_conversions,
    reason = "the `as` conversion from `u32` to `usize` is safe"
)]
#[expect(
    clippy::arithmetic_side_effects,
    reason = "`end` cannot reasonably overflow here"
)]
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
        .take_while(|&(_, c)| is_word_char(c))
        .last()
        .map_or(byte_pos, |(i, _)| i);

    // Scan right from the cursor position to find the end of the word.
    let end = byte_pos
        + line[byte_pos..]
            .char_indices()
            .take_while(|&(_, c)| is_word_char(c))
            .last()
            .map_or(0, |(i, c)| i + c.len_utf8());

    (start < end).then(|| line[start..end].to_string())
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
    let pattern = if whole_word {
        // The word-boundary assertion prevents "for" from matching inside "format".
        format!(r"\b{escaped}\b")
    } else {
        escaped
    };
    RegexBuilder::new(&pattern)
        .case_insensitive(ignore_case)
        .build()
        .ok()
}

/// Entry point of the LSP server.
///
/// LSP servers communicate over `stdin`/`stdout`; tower-lsp handles the JSON-RPC framing and dispatches each message to
/// the appropriate handler on the [`Backend`]. The server runs until `stdin` is closed (i.e., until the editor exits or
/// restarts the language server).
#[tokio::main]
async fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}

/// Unit tests.
#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests can use `unwrap`")]
#[expect(
    clippy::non_ascii_literal,
    reason = "test data contains non-ASCII characters"
)]
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
        assert!(
            !s.has_any(),
            "case-insensitive toggle of the same word must leave no highlights"
        );
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
    fn utf16_to_byte_inside_surrogate_pair_is_none() {
        // "😀a": '😀' occupies 2 UTF-16 units but 4 UTF-8 bytes.
        // UTF-16 offset 1 -> lands inside the surrogate pair.
        assert_eq!(utf16_to_byte("😀a", 1), None);
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
    fn is_highlightable_nonword_chars_in_whole_word_mode_false() {
        assert!(!is_highlightable(".", true));
        assert!(!is_highlightable("()", true));
    }

    #[test]
    fn is_highlightable_any_nonempty_without_whole_word_mode_true() {
        assert!(is_highlightable("foo", false));
        assert!(is_highlightable("(bar)", false));
        assert!(is_highlightable("foo bar", false));
    }

    #[test]
    fn is_highlightable_identifier_in_whole_word_mode_true() {
        assert!(is_highlightable("foo", true));
        assert!(is_highlightable("foo_bar", true));
        assert!(is_highlightable("foo123", true));
        assert!(is_highlightable("_private", true));
    }

    #[test]
    fn is_highlightable_leading_nonword_char_in_whole_word_mode_false() {
        assert!(!is_highlightable("(foo", true));
        assert!(!is_highlightable(".foo", true));
        assert!(!is_highlightable(" foo", true));
    }

    #[test]
    fn is_highlightable_trailing_nonword_char_in_whole_word_mode_false() {
        assert!(!is_highlightable("foo(", true));
        assert!(!is_highlightable("foo.", true));
        assert!(!is_highlightable("foo ", true));
    }

    #[test]
    fn is_highlightable_middle_nonword_char_always_true() {
        assert!(is_highlightable("foo.bar", true));
        assert!(is_highlightable("foo.bar", false));
    }

    #[test]
    fn is_highlightable_single_word_char_true() {
        assert!(is_highlightable("x", true));
        assert!(is_highlightable("_", true));
        assert!(is_highlightable("1", true));
    }

    // TODO: The behavior of `is_highlightable` without whole-word mode with non-word characters is somewhat debatable.
    // We should probably refine it if `whole_word` ever becomes user-configurable. Leave as-is for the time being.
    #[test]
    fn is_highlightable_any_nonempty_selection_without_whole_word_mode_true() {
        assert!(is_highlightable(" ", false));
        assert!(is_highlightable(".", false));
        assert!(is_highlightable("()", false));
        assert!(is_highlightable("foo bar", false));
    }

    // Test `compile_word_regex`.

    #[test]
    fn compile_word_regex_whole_word_and_ignore_case_both_disabled() {
        let mut re = compile_word_regex("for", false, false).unwrap();
        assert!(re.is_match("for"), "basic match must work");
        assert!(
            re.is_match("format"),
            "non-whole-word must match when whole-word disabled"
        );

        re = compile_word_regex("For", false, false).unwrap();
        assert!(
            re.is_match("For"),
            "case-sensitive match must work when ignore_case disabled"
        );
        assert!(
            !re.is_match("for"),
            "case-sensitive match must work when ignore_case disabled"
        );
        assert!(
            !re.is_match("FOR"),
            "case-sensitive match must work when ignore_case disabled"
        );
    }

    #[test]
    fn compile_word_regex_case_insensitive_flag_enabled() {
        let re = compile_word_regex("Foo", false, true).unwrap();
        assert!(
            re.is_match("Foo"),
            "case-insensitive match must work when ignore_case enabled"
        );
        assert!(
            re.is_match("foo"),
            "case-insensitive match must work when ignore_case enabled"
        );
        assert!(
            re.is_match("FOO"),
            "case-insensitive match must work when ignore_case enabled"
        );
    }

    #[test]
    fn compile_word_regex_whole_word_flag_enabled() {
        let re = compile_word_regex("for", true, false).unwrap();
        assert!(
            re.is_match("for x in y"),
            "whole-word 'for' must match standalone"
        );
        assert!(
            re.is_match("(for)"),
            "whole-word 'for' must match when surrounded by non-word chars"
        );
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
    fn compile_word_regex_whole_word_and_ignore_case_both_enabled() {
        let re = compile_word_regex("Foo", true, true).unwrap();
        assert!(
            re.is_match("foo"),
            "case-insensitive whole-word must match lowercase"
        );
        assert!(
            re.is_match("FOO"),
            "case-insensitive whole-word must match uppercase"
        );
        assert!(
            !re.is_match("foobar"),
            "whole-word must not match 'Foo' inside 'foobar'"
        );
        assert!(
            !re.is_match("FOOBAR"),
            "whole-word must not match 'FOO' inside 'FOOBAR'"
        );
    }

    #[test]
    fn compile_word_regex_escapes_special_chars() {
        let mut re = compile_word_regex("foo.bar", false, false).unwrap();
        assert!(re.is_match("foo.bar"));
        assert!(
            !re.is_match("fooXbar"),
            "dot must match literally, not as any-char"
        );

        re = compile_word_regex("foo()", false, false).unwrap();
        assert!(re.is_match("foo()"));
        assert!(!re.is_match("foo"), "parentheses are not optional");

        re = compile_word_regex("a*b", false, false).unwrap();
        assert!(re.is_match("a*b"));
        assert!(!re.is_match("ab"), "star must be literal, not a quantifier");

        re = compile_word_regex("a+b", false, false).unwrap();
        assert!(re.is_match("a+b"));
        assert!(!re.is_match("ab"), "plus must be literal, not a quantifier");
        assert!(
            !re.is_match("aab"),
            "plus must be literal, not a quantifier"
        );

        re = compile_word_regex("a?b", false, false).unwrap();
        assert!(re.is_match("a?b"));
        assert!(
            !re.is_match("ab"),
            "question mark must be literal, not optional"
        );
        assert!(
            !re.is_match("b"),
            "question mark must be literal, not optional"
        );

        re = compile_word_regex("a|b", false, false).unwrap();
        assert!(re.is_match("a|b"));
        assert!(!re.is_match("a"), "pipe must be literal, not alternation");
        assert!(!re.is_match("b"), "pipe must be literal, not alternation");
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

    #[test]
    fn matches_anywhere_whole_word_and_ignore_case_both_enabled() {
        assert!(
            matches_anywhere("let Foo = 1;", "foo", true, true),
            "'Foo' should match 'foo' with whole_word and ignore_case both enabled"
        );
        assert!(
            !matches_anywhere("Format()", "for", true, true),
            "'for' should not match inside 'Format' even with ignore_case enabled when whole_word is enabled"
        );
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
    fn word_at_selection_middle_nonword_char_is_fine() {
        // "let foo.bar = 1;" - select "foo.bar" at chars 4..11
        let range = make_range(0, 4, 0, 11);
        assert_eq!(
            word_at("let foo.bar = 1;", range),
            Some("foo.bar".to_owned())
        );
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
    fn word_at_cursor_on_word_with_underscores() {
        // "some_var = 1;" - cursor on 'v' (char 5).
        assert_eq!(
            word_at("some_var = 1;", cursor_range(0, 5)),
            Some("some_var".to_owned())
        );
    }

    #[test]
    fn word_at_single_char_word() {
        // "a b" - cursor on 'a' (char 0) -> word "a".
        assert_eq!(word_at("a b", cursor_range(0, 0)), Some("a".to_owned()));
    }

    #[test]
    fn word_at_end_of_string_no_trailing_space() {
        // "hello" with no trailing space - the right-scan must not overshoot the string end.
        assert_eq!(
            word_at("hello", cursor_range(0, 0)),
            Some("hello".to_owned())
        );
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

    #[test]
    fn word_at_unicode_alphanumeric_char() {
        // "café": 'é' (U+00E9) is alphanumeric, so it's a word character. Cursor at UTF-16
        // offset 3 (on 'é') must return the full word "café".
        assert_eq!(word_at("café", cursor_range(0, 3)), Some("café".to_owned()));
    }
}

/// Integration tests.
#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests can use `unwrap`")]
#[expect(
    clippy::non_ascii_literal,
    reason = "test data contains non-ASCII characters"
)]
mod integration {
    use tower::{Service as _, ServiceExt as _};
    use tower_lsp::jsonrpc;

    use super::*;

    /// Service type used across all integration tests.
    type Svc = LspService<Backend>;

    /// Stable document URI reused by all tests; the service is fresh per test so there's no cross-test state.
    const URI: &str = "file:///test.txt";

    /// A second stable document URI for multi-document tests.
    const URI2: &str = "file:///test2.txt";

    // Helper functions.

    /// Serializes `req` as a JSON-RPC request, drives it through the service, and returns the serialized response.
    /// Notifications (no `id` field) produce `None`; requests produce `Some(response_json)`.
    #[expect(clippy::shadow_reuse, reason = "shadowing is convenient here")]
    async fn call_inner(svc: &mut Svc, req: serde_json::Value) -> Option<serde_json::Value> {
        let req: jsonrpc::Request = serde_json::from_value(req).unwrap();
        let res = svc.ready().await.unwrap().call(req).await.unwrap();
        res.map(|r| serde_json::to_value(r).unwrap())
    }

    /// Creates a fresh service and completes the mandatory LSP handshake (`initialize` -> `initialized`).
    /// The `ClientSocket` (used for server-to-client notifications) is dropped immediately; the backend
    /// ignores send errors with `let _ =`, so this is safe and avoids keeping a handle we don't need.
    async fn make_service() -> Svc {
        let (mut svc, socket) = LspService::new(Backend::new);
        drop(
            call_inner(
                &mut svc,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 0_i32,
                    "method": "initialize",
                    "params": { "capabilities": {} }
                }),
            )
            .await,
        );
        drop(
            call_inner(
                &mut svc,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "initialized",
                    "params": {}
                }),
            )
            .await,
        );
        drop(socket);
        svc
    }

    /// Registers a document via `textDocument/didOpen` so it's available in `state.docs`.
    async fn open(svc: &mut Svc, uri: &str, text: &str) {
        drop(
            call_inner(
                svc,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/didOpen",
                    "params": {
                        "textDocument": {
                            "uri": uri,
                            "languageId": "plaintext",
                            "version": 1_i32,
                            "text": text
                        }
                    }
                }),
            )
            .await,
        );
    }

    /// Replaces a document's full text via `textDocument/didChange` (FULL sync: one change, no range).
    async fn change(svc: &mut Svc, uri: &str, text: &str) {
        drop(
            call_inner(
                svc,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/didChange",
                    "params": {
                        "textDocument": { "uri": uri, "version": 2_i32 },
                        "contentChanges": [{ "text": text }]
                    }
                }),
            )
            .await,
        );
    }

    /// Evicts a document from `state.docs` via `textDocument/didClose`.
    async fn close(svc: &mut Svc, uri: &str) {
        drop(
            call_inner(
                svc,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/didClose",
                    "params": {
                        "textDocument": { "uri": uri }
                    }
                }),
            )
            .await,
        );
    }

    /// Toggles a word on/off via `workspace/executeCommand` -> `zed-highlight.toggle`.
    /// (`id` must be unique per test to satisfy the JSON-RPC request/response pairing).
    async fn toggle(svc: &mut Svc, id: u32, word: &str) {
        drop(
            call_inner(
                svc,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "workspace/executeCommand",
                    "params": {
                        "command": "zed-highlight.toggle",
                        "arguments": [word]
                    }
                }),
            )
            .await,
        );
    }

    /// Removes all highlighted words via `workspace/executeCommand` -> `zed-highlight.clear`.
    async fn clear(svc: &mut Svc, id: u32) {
        drop(
            call_inner(
                svc,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "workspace/executeCommand",
                    "params": {
                        "command": "zed-highlight.clear"
                    }
                }),
            )
            .await,
        );
    }

    /// Requests code actions at the given cursor position and returns their titles in order.
    async fn code_action(
        svc: &mut Svc,
        id: u32,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Vec<String> {
        let res = call_inner(
            svc,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "textDocument/codeAction",
                "params": {
                    "textDocument": { "uri": uri },
                    "range": {
                        "start": { "line": line, "character": character },
                        "end":   { "line": line, "character": character }
                    },
                    "context": { "diagnostics": [] }
                }
            }),
        )
        .await
        .unwrap();
        res["result"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v["title"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Requests code actions for a given selection range and returns their titles in order.
    async fn code_action_range(
        svc: &mut Svc,
        id: u32,
        uri: &str,
        start_line: u32,
        start_char: u32,
        end_line: u32,
        end_char: u32,
    ) -> Vec<String> {
        let res = call_inner(
            svc,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "textDocument/codeAction",
                "params": {
                    "textDocument": { "uri": uri },
                    "range": {
                        "start": { "line": start_line, "character": start_char },
                        "end":   { "line": end_line,   "character": end_char }
                    },
                    "context": { "diagnostics": [] }
                }
            }),
        )
        .await
        .unwrap();
        res["result"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v["title"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Requests the full semantic token list for a document and returns the raw flat `data` array. Each token is
    /// encoded as 5 consecutive u32s: `delta_line`, `delta_start`, `length`, `token_type`, `token_modifiers`.
    async fn get_tokens(svc: &mut Svc, id: u32, uri: &str) -> Vec<u32> {
        let res = call_inner(
            svc,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "textDocument/semanticTokens/full",
                "params": {
                    "textDocument": { "uri": uri }
                }
            }),
        )
        .await
        .unwrap();
        res["result"]["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| u32::try_from(v.as_u64().unwrap()).unwrap())
            .collect()
    }

    /// Converts the flat token array into (`delta_line`, `delta_start`, `length`, `token_type`) 4-tuples,
    /// dropping the always-zero `token_modifiers_bitset` field.
    #[expect(
        clippy::missing_asserts_for_indexing,
        reason = "this negligible performance hit is not important in tests"
    )]
    fn decode_tokens(data: &[u32]) -> Vec<(u32, u32, u32, u32)> {
        data.chunks_exact(5)
            .map(|c| (c[0], c[1], c[2], c[3]))
            .collect()
    }

    // Tests.

    /// Baseline: no highlighted words -> no tokens emitted.
    #[tokio::test]
    async fn tokens_empty_without_highlighted_words() {
        let mut svc = make_service().await;
        open(&mut svc, URI, "hello world").await;
        let data = get_tokens(&mut svc, 1, URI).await;
        assert!(data.is_empty(), "no tokens when no words are highlighted");
    }

    /// End-to-end smoke test: one word toggled, one occurrence in document -> verify exact 5-tuple encoding.
    #[tokio::test]
    async fn tokens_single_word_single_occurrence() {
        let mut svc = make_service().await;
        open(&mut svc, URI, "hello world").await;
        toggle(&mut svc, 1, "hello").await;
        let data = get_tokens(&mut svc, 2, URI).await;
        assert_eq!(
            data,
            [0, 0, 5, 0, 0],
            "one token: delta_line=0, delta_start=0, len=5, type=0, mods=0"
        );
    }

    /// When two tokens share a line, the second token's `delta_start` is relative to the first token's start column.
    #[tokio::test]
    async fn tokens_same_line_delta_encoding() {
        let mut svc = make_service().await;
        open(&mut svc, URI, "foo foo").await;
        toggle(&mut svc, 1, "foo").await;
        let data = get_tokens(&mut svc, 2, URI).await;
        assert_eq!(
            data,
            [0, 0, 3, 0, 0, 0, 4, 3, 0, 0],
            "second token delta_start is relative to first (4 chars apart on same line)"
        );
    }

    /// When a token is on a different line than the previous one, `delta_start` resets to the absolute column rather
    /// than being relative to the previous token. This is mandated by the LSP spec.
    #[tokio::test]
    async fn tokens_cross_line_delta_encoding() {
        let mut svc = make_service().await;
        open(&mut svc, URI, "foo\nfoo").await;
        toggle(&mut svc, 1, "foo").await;
        let data = get_tokens(&mut svc, 2, URI).await;
        assert_eq!(
            data,
            [0, 0, 3, 0, 0, 1, 0, 3, 0, 0],
            "cross-line token resets delta_start to absolute column"
        );
    }

    /// Each word occupies its own slot in `state.words`; the slot index maps to a distinct token type, which Zed
    /// resolves to a different highlight color via `semantic_token_rules`.
    #[tokio::test]
    async fn tokens_two_words_get_distinct_types() {
        let mut svc = make_service().await;
        open(&mut svc, URI, "foo bar").await;
        toggle(&mut svc, 1, "foo").await;
        toggle(&mut svc, 2, "bar").await;
        let data = get_tokens(&mut svc, 3, URI).await;
        let decoded = decode_tokens(&data);
        assert_eq!(
            decoded,
            [(0, 0, 3, 0), (0, 4, 3, 1)],
            "first toggled word gets type 0, second gets type 1"
        );
    }

    /// A second toggle on the same word soft-deletes it (sets its slot to `None`); `build_tokens` skips `None` slots.
    #[tokio::test]
    async fn tokens_toggled_off_word_produces_no_tokens() {
        let mut svc = make_service().await;
        open(&mut svc, URI, "hello world").await;
        toggle(&mut svc, 1, "hello").await;
        toggle(&mut svc, 2, "hello").await;
        let data = get_tokens(&mut svc, 3, URI).await;
        assert!(data.is_empty(), "toggled-off word must produce no tokens");
    }

    /// `zed-highlight.clear` calls `words_clear`, which drops all slots at once.
    #[tokio::test]
    async fn tokens_clear_removes_all() {
        let mut svc = make_service().await;
        open(&mut svc, URI, "hello world").await;
        toggle(&mut svc, 1, "hello").await;
        toggle(&mut svc, 2, "world").await;
        clear(&mut svc, 3).await;
        let data = get_tokens(&mut svc, 4, URI).await;
        assert!(data.is_empty(), "clear must remove all highlighted words");
    }

    /// `token type = slot_index % NUM_COLORS` (8), so the 9th word wraps back to type 0 and shares a color with the
    /// first word.
    #[tokio::test]
    async fn tokens_color_wraps_past_num_colors() {
        let mut svc = make_service().await;
        open(&mut svc, URI, "w0 w1 w2 w3 w4 w5 w6 w7 w8").await;
        for (id, w) in (1_u32..).zip(["w0", "w1", "w2", "w3", "w4", "w5", "w6", "w7", "w8"]) {
            toggle(&mut svc, id, w).await;
        }
        let data = get_tokens(&mut svc, 10, URI).await;
        assert_eq!(data.len(), 9 * 5, "nine tokens, each with 5 fields");
        assert_eq!(data[3], 0, "first word (w0) gets token type 0");
        assert_eq!(data[43], 0, "ninth word (w8) wraps back to token type 0");
    }

    /// Default whole-word mode wraps the pattern in `\b...\b`, so "foo" does not match inside "foobar".
    #[tokio::test]
    async fn tokens_whole_word_excludes_substrings() {
        let mut svc = make_service().await;
        open(&mut svc, URI, "foobar foo").await;
        toggle(&mut svc, 1, "foo").await;
        let data = get_tokens(&mut svc, 2, URI).await;
        assert_eq!(
            data,
            [0, 7, 3, 0, 0],
            "whole-word mode must not match `foo` inside `foobar`; only standalone `foo` at col 7"
        );
    }

    /// `textDocument/didChange` replaces the stored document text, so `build_tokens` sees the new content on the very
    /// next `semanticTokens/full` request.
    #[tokio::test]
    async fn tokens_did_change_updates_document() {
        let mut svc = make_service().await;
        open(&mut svc, URI, "hello world").await;
        toggle(&mut svc, 1, "foo").await;
        let data1 = get_tokens(&mut svc, 2, URI).await;
        assert!(
            data1.is_empty(),
            "initially no tokens because document lacks `foo`"
        );
        change(&mut svc, URI, "foo bar").await;
        let data2 = get_tokens(&mut svc, 3, URI).await;
        assert_eq!(
            data2,
            [0, 0, 3, 0, 0],
            "token appears after document is updated with `foo`"
        );
    }

    /// `textDocument/didClose` removes the document from `state.docs`; `build_tokens` returns an empty list when the
    /// document is absent rather than panicking or returning stale data.
    #[tokio::test]
    async fn tokens_did_close_evicts_document() {
        let mut svc = make_service().await;
        open(&mut svc, URI, "hello world").await;
        toggle(&mut svc, 1, "hello").await;
        let data1 = get_tokens(&mut svc, 2, URI).await;
        assert!(!data1.is_empty(), "token present before close");
        close(&mut svc, URI).await;
        let data2 = get_tokens(&mut svc, 3, URI).await;
        assert!(data2.is_empty(), "evicted document returns no tokens");
    }

    /// The toggle code action must use a stateless title ("Toggle highlight") that stays accurate regardless of when
    /// Zed last fetched the response. Zed caches code actions by cursor position and only invalidates that cache on
    /// cursor movement or document edits, so a state-dependent title ("Highlight" vs "Remove highlight") would be
    /// stale and misleading after a toggle without cursor movement. The stateless title is the server-side workaround
    /// for the missing `workspace/codeAction/refresh` support in Zed.
    #[tokio::test]
    async fn code_action_title_is_stateless() {
        let mut svc = make_service().await;
        open(&mut svc, URI, "foo bar").await;

        // Before and after toggling, the code action title must be the same stateless string.
        let actions_before = code_action(&mut svc, 1, URI, 0, 0).await;
        toggle(&mut svc, 2, "foo").await;
        let actions_after = code_action(&mut svc, 3, URI, 0, 0).await;
        toggle(&mut svc, 4, "foo").await;
        let actions_after2 = code_action(&mut svc, 5, URI, 0, 0).await;

        let expected = r#"Toggle highlight: "foo""#;
        assert!(
            actions_before.iter().any(|a| a == expected),
            "expected stateless title before any toggle; got: {actions_before:?}"
        );
        assert!(
            actions_after.iter().any(|a| a == expected),
            "expected stateless title after toggle on; got: {actions_after:?}"
        );
        assert!(
            actions_after2.iter().any(|a| a == expected),
            "expected stateless title after toggle off; got: {actions_after2:?}"
        );
    }

    /// The LSP spec requires character offsets in UTF-16 code units, not bytes. '中' and '文' are each 1 UTF-16 unit
    /// but 3 UTF-8 bytes, so "foo" at UTF-16 offset 3 must not be reported at byte offset 7.
    #[tokio::test]
    async fn tokens_utf16_offsets_with_multibyte() {
        let mut svc = make_service().await;
        open(&mut svc, URI, "中文 foo").await;
        toggle(&mut svc, 1, "foo").await;
        let data = get_tokens(&mut svc, 2, URI).await;
        assert_eq!(
            data,
            [0, 3, 3, 0, 0],
            "delta_start must be UTF-16 offset (3), not byte offset (7)"
        );
    }

    /// Cursor on a space (non-word character) must produce no code actions when no words are highlighted.
    #[tokio::test]
    async fn code_action_no_actions_when_cursor_on_space() {
        let mut svc = make_service().await;
        open(&mut svc, URI, "foo bar").await;
        // Character 3 is the space between "foo" and "bar".
        let actions = code_action(&mut svc, 1, URI, 0, 3).await;
        assert!(actions.is_empty(), "no actions when cursor is on a space");
    }

    /// The "Clear all highlights" action must not appear when no words are currently highlighted.
    #[tokio::test]
    async fn code_action_no_clear_action_when_no_highlights() {
        let mut svc = make_service().await;
        open(&mut svc, URI, "foo").await;
        let actions = code_action(&mut svc, 1, URI, 0, 0).await;
        assert!(
            actions.iter().any(|a| a.contains("Toggle highlight")),
            "toggle action must be present when cursor is on a valid word"
        );
        assert!(
            !actions.iter().any(|a| a.contains("Clear")),
            "clear action must not appear when no words are highlighted"
        );
    }

    /// After toggling a word on, both the toggle and clear actions must be offered together.
    #[tokio::test]
    async fn code_action_both_actions_when_word_highlighted() {
        let mut svc = make_service().await;
        open(&mut svc, URI, "foo bar").await;
        toggle(&mut svc, 1, "foo").await;
        let actions = code_action(&mut svc, 2, URI, 0, 0).await;
        assert!(
            actions.iter().any(|a| a.contains("Toggle highlight")),
            "toggle action must be present"
        );
        assert!(
            actions.iter().any(|a| a.contains("Clear")),
            "clear action must appear when at least one word is highlighted"
        );
    }

    /// In whole-word mode (the default), toggling a word that starts or ends with a non-word character
    /// must be a no-op: `is_highlightable` rejects it, the words list must not change, and no tokens
    /// must be produced.
    #[tokio::test]
    async fn toggle_non_highlightable_word_is_no_op() {
        let mut svc = make_service().await;
        open(&mut svc, URI, "foo . bar").await;
        // "." starts and ends with a non-word character — `is_highlightable(".", true)` is false.
        toggle(&mut svc, 1, ".").await;
        let data = get_tokens(&mut svc, 2, URI).await;
        assert!(
            data.is_empty(),
            "toggling a non-highlightable word must not add it to the highlight list"
        );
    }

    /// After a word is removed, the next new word reuses the freed slot and therefore inherits its
    /// color index rather than being appended at the end with a new index.
    #[tokio::test]
    async fn tokens_color_reuse_after_remove() {
        let mut svc = make_service().await;
        open(&mut svc, URI, "baz bar").await;
        toggle(&mut svc, 1, "foo").await; // slot 0 = Some("foo"), color 0
        toggle(&mut svc, 2, "bar").await; // slot 1 = Some("bar"), color 1
        toggle(&mut svc, 3, "foo").await; // slot 0 = None (soft-delete)
        toggle(&mut svc, 4, "baz").await; // slot 0 = Some("baz"), reuses color 0
        let data = get_tokens(&mut svc, 5, URI).await;
        let decoded = decode_tokens(&data);
        assert_eq!(
            decoded,
            [(0, 0, 3, 0), (0, 4, 3, 1)],
            "baz must reuse slot 0 (color 0) after foo was removed; bar must keep slot 1 (color 1)"
        );
    }

    /// Highlights are global across all open documents: toggling a word must produce tokens in every
    /// document that contains it, not only the one where the action was invoked.
    #[tokio::test]
    async fn tokens_multiple_documents() {
        let mut svc = make_service().await;
        open(&mut svc, URI, "hello world").await;
        open(&mut svc, URI2, "hello there").await;
        toggle(&mut svc, 1, "hello").await;
        let data1 = get_tokens(&mut svc, 2, URI).await;
        let data2 = get_tokens(&mut svc, 3, URI2).await;
        assert!(
            !data1.is_empty(),
            "first document must have a token for 'hello'"
        );
        assert!(
            !data2.is_empty(),
            "second document must have a token for 'hello'"
        );
        assert_eq!(data1[3], 0, "token type in first document must be 0");
        assert_eq!(data2[3], 0, "token type in second document must be 0");
    }

    /// An empty document must produce no tokens even when words are highlighted.
    #[tokio::test]
    async fn tokens_empty_document_returns_no_tokens() {
        let mut svc = make_service().await;
        open(&mut svc, URI, "").await;
        toggle(&mut svc, 1, "foo").await;
        let data = get_tokens(&mut svc, 2, URI).await;
        assert!(
            data.is_empty(),
            "empty document must produce no tokens even if words are highlighted"
        );
    }

    /// Emoji (U+1F600) occupy two UTF-16 code units (a surrogate pair). A token following an emoji
    /// must report the correct UTF-16 `delta_start`, not the byte offset.
    #[tokio::test]
    async fn tokens_surrogate_pair_utf16_offset() {
        let mut svc = make_service().await;
        // '😀' is U+1F600: 4 UTF-8 bytes but 2 UTF-16 code units.
        // "foo" therefore starts at UTF-16 offset 2, not byte offset 4.
        open(&mut svc, URI, "😀foo").await;
        toggle(&mut svc, 1, "foo").await;
        let data = get_tokens(&mut svc, 2, URI).await;
        assert_eq!(
            data,
            [0, 2, 3, 0, 0],
            "delta_start must be UTF-16 offset (2), not byte offset (4)"
        );
    }

    /// A non-empty single-line selection must be used verbatim as the highlight target, bypassing the
    /// cursor-to-word-boundary scan. This matters for selections that include non-word characters such
    /// as dots that the cursor scan would not span.
    #[tokio::test]
    async fn code_action_selection_uses_selected_text() {
        let mut svc = make_service().await;
        // "foo.bar" selected in full (chars 0..7). The dot makes the cursor scan stop at "foo",
        // but the selection path must return the full selected text.
        open(&mut svc, URI, "foo.bar").await;
        let actions = code_action_range(&mut svc, 1, URI, 0, 0, 0, 7).await;
        assert!(
            actions
                .iter()
                .any(|a| a == r#"Toggle highlight: "foo.bar""#),
            "selection must yield a toggle action for the full selected text; got: {actions:?}"
        );
    }
}

# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Repository layout

Two-crate Cargo workspace:

- Root crate (`src/lib.rs`) — the Zed extension itself. Compiles to `cdylib` (WASM, `extension.wasm`) via `zed_extension_api`. Its sole job is to locate or download the LSP server binary and tell Zed how to launch it.
- `lsp/` (`lsp/src/main.rs`) — the `zed-highlight-lsp` binary, a tower-lsp / tokio language server that does all the real work (matching, semantic token generation, code actions).

The extension WASM and the LSP binary are independent build artifacts. End users install the extension; the extension auto-downloads the LSP from GitHub Releases on first use (or reuses a binary found on `$PATH` via `worktree.which`, which is what enables dev builds).

## Commands

```sh
# Format / lint / build (workspace-wide)
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo build
cargo test

# Install the LSP locally for dev (so the extension uses it instead of downloading)
cargo install --path lsp

# Install the dev extension in Zed: run `zed: install dev extension` from the command palette
# and select this repo's directory.
```

Three CI workflows live in `.github/workflows/`:

- `build.yml` — runs on every push to `master`: fmt, clippy (warnings-as-errors), test, and build on Linux; test+build on macOS and Windows. Match these locally before pushing.
- `release.yml` — triggers on `v*` tags: cross-compiles the LSP binary for all platforms and publishes a GitHub Release with the archives.
- `publish.yml` — triggers on `v*` tags: opens a PR against `zed-industries/extensions` to bump the extension version in their registry. Requires a `ZED_EXTENSIONS_TOKEN` secret (GitHub PAT with `repo` scope).

## Lint posture

The workspace `[lints]` table in `Cargo.toml` enables clippy `all`, `pedantic`, `nursery`, `cargo`, plus a long list of restriction lints (`unwrap_used`, `expect_used`, `panic`, `todo`, `unreachable`, `exhaustive_enums`/`structs`, `missing_docs_in_private_items`, `undocumented_unsafe_blocks`, `str_to_string`, etc.). Treat these as part of the contract:

- Every item, including private ones, needs a doc comment.
- Don't add `unwrap()`, `expect()`, `panic!`, `todo!`, `unreachable!`, or `dbg!` — clippy will fail CI.
- When suppressing a lint, prefer `#[expect(..., reason = "...")]` over `#[allow(...)]` and always include a reason (`allow_attributes_without_reason` is on).
- Errors that must be ignored should be matched explicitly (e.g. `let _ = ...`) only with an `#[expect(clippy::let_underscore_must_use, reason = "...")]` block.

## Architecture notes

### Extension side (`src/lib.rs`)

`HighlightExtension::language_server_command` runs on every LSP launch. It first calls `worktree.which(BINARY_NAME)` so a locally-built `zed-highlight-lsp` on `$PATH` wins over the GitHub-downloaded one. The download path (`install_binary`) is wrapped by `ensure_binary`, which is responsible for reporting `LanguageServerInstallationStatus::Failed` to Zed on error — otherwise the UI gets stuck on `CheckingForUpdate`/`Downloading`. Cached binary directories are versioned (`zed-highlight-lsp-<version>/`) and old ones are pruned on successful install.

### LSP side (`lsp/src/main.rs`)

State model: a single shared `State` (behind `tokio::sync::Mutex`) holds the list of highlighted words and a `HashMap<Url, String>` of full document contents. Highlights are global across all open documents — toggling a word in one file affects all files.

Word slot semantics: `words: Vec<Option<String>>`. Soft-delete leaves `None` in place so existing colors don't shift when a word is removed; new words reuse the first `None` slot before growing the Vec. The visible color index is `slot_index % NUM_COLORS` (8), so a 9th simultaneous highlight reuses color 0.

Refresh model: state changes don't push tokens directly. They call either `immediate_refresh` (user actions like toggle/clear — cancel any pending debounce, send `workspace/semanticTokens/refresh` now) or `debounced_refresh` (document edits — coalesce rapid edits into a single refresh after `DEBOUNCE_DELAY_MS = 250ms`). Zed then re-requests via `semantic_tokens_full`, which calls `build_tokens` to do the scan. This means `did_open`/`did_change` must populate `state.docs` _before_ scheduling the refresh; the debounce window also acts as a safety net for races.

Token encoding: `build_tokens` collects absolute `(line, start, length, token_type)` matches, sorts them, then converts to the LSP delta encoding (`delta_start` resets to absolute whenever `delta_line > 0`). Character offsets are in **UTF-16 code units** per the LSP spec — see `utf16_len` and `utf16_to_byte`. Don't accidentally use byte offsets when interacting with `Range`/`Position`.

Matching: words are compiled to a `Regex` via `compile_word_regex`. Default flags are `whole_word = true`, `ignore_case = false`. `whole_word` uses `\b<escaped>\b`, which means a candidate whose first/last char isn't a word char would compile to a never-matching regex — `is_highlightable` filters those out at the code-action layer so they never enter `words` invisibly. `matches_anywhere` checks the current document before exposing the toggle action so we don't offer a no-op.

Word resolution: `word_at` handles two cases — non-empty single-line selection uses the selection verbatim; cursor-only (or multi-line selection) scans `\w`-class chars left/right from the cursor. "Word char" is `is_alphanumeric() || '_'`.

Commands: only two are registered with Zed (`zed-highlight.toggle`, `zed-highlight.clear`). Both are surfaced via `code_action`, not bound to keymaps — users invoke them via `editor: toggle code actions` (`⌘.`).

Code action title: the toggle action always uses the stateless label `Toggle highlight: "<word>"` rather than a state-dependent `Highlight` / `Remove highlight`. Zed caches code action responses by cursor position and only invalidates that cache on cursor movement or document edits — it does not implement `workspace/codeAction/refresh`. A stateless title is always accurate regardless of when Zed last fetched the response, avoiding the confusing mismatch of seeing `Highlight: "foo"` when the word is already highlighted (or vice versa) without the user having moved the cursor. Don't revert this to a state-dependent title without first verifying that Zed has added `workspace/codeAction/refresh` support.

### Adding a new supported language

Add it to the `languages = [...]` array in `extension.toml`. The LSP itself is language-agnostic (it operates on text and word boundaries), so no LSP-side changes are needed.

### Configuration surface

The 8 colors are not shipped by the extension — users configure them under `global_lsp_settings.semantic_token_rules` in their Zed `settings.json`, mapping the token type names `zed-highlight-0` through `zed-highlight-7` to colors. `semantic_tokens` must be set to `"combined"` or `"full"` for highlights to appear. Sample dark/light schemes are in `README.md`.

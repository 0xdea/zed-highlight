# Changelog for zed-highlight

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Update documentation.
- Update dependencies.

## [0.1.4] - 2026-08-08

First release of the extension to be published in Zed's [marketplace](https://zed.dev/extensions/word-highlight-lsp).

### Changed

- Update the extension ID to `word-highlight-lsp` to comply with Zed's marketplace naming requirements.
- Improve CI.
- Update dependencies.

## [0.1.3] - 2026-06-20

### Added

- Add some unit and integration tests.

### Changed

- Rename the extension to Word Highlight to make the name more descriptive.
- Pin `tokio` version.
- Enable all clippy restriction lints and fix any resulting issues.
- Improve comments.
- Update documentation.
- Update dependencies.

### Removed

- Remove the draft GitHub Actions workflow for publishing the extension to Zed's marketplace.

## [0.1.2] - 2026-05-27

### Changed

- Move the directory pruning step in the extension so that any stale directories are self-healed.
- Use `Arc` in `State::docs` to avoid cloning a full document under the lock in LSP server's `build_tokens`.
- Use `RegexBuilder` instead of `Regex` for readability.
- Improve CI workflows.
- Improve the example color scheme for light themes in the documentation.
- Update documentation.
- Update dependencies.

### Fixed

- Compute the correct path to the LSP server binary in the Windows extension.

## [0.1.1] - 2026-05-26

### Added

- Report `LanguageServerInstallationStatus::Failed` to Zed in case of errors in the install process.
- Add some supported languages to the extension manifest.
- Add unit tests for the extension and the LSP server with the help of my friend Claude.
- Add integration tests for the LSP server, also with the help of my friend Claude.
- Add a GitHub Actions workflow to run tests for the extension and the LSP server on push events.
- Add a GitHub Actions workflow to publish LSP server binaries to GitHub Releases on new version tags.
- Draft a GitHub Actions workflow to publish the extension to Zed's marketplace on new version tags.

### Changed

- Rename Zed Highlight to Highlight to follow the naming convention of Zed extensions.
- Specify stricter version requirements for dependencies.
- Update documentation and polish everything for release.

### Fixed

- Use a stateless `Toggle highlight` code action title to prevent stale code actions after toggling highlights.

## [0.1.0] - 2026-05-10

- First pre-release of the LSP to be published to [crates.io](https://crates.io/).

[unreleased]: https://github.com/0xdea/zed-highlight/compare/v0.1.4...HEAD
[0.1.4]: https://github.com/0xdea/zed-highlight/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/0xdea/zed-highlight/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/0xdea/zed-highlight/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/0xdea/zed-highlight/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/0xdea/zed-highlight/releases/tag/v0.1.0

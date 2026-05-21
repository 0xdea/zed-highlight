# Changelog for zed-highlight

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Report `LanguageServerInstallationStatus::Failed` to Zed in case of errors in the install process.
- Add some supported languages to the extension manifest.
- Add a GitHub Actions workflow to publish LSP server binaries to GitHub Releases on new version tags.
- Add unit tests for the extension and the LSP server with the help of my friend Claude.

### Changed

- Rename Zed Highlight to Highlight to follow the naming convention of Zed extensions.
- Specify stricter version requirements for dependencies.
- Update documentation and polish everything for release.

## [0.1.0] - 2026-05-10

- First pre-release of the LSP to be published to [crates.io](https://crates.io/).

[unreleased]: https://github.com/0xdea/zed-highlight/compare/v0.1.0...HEAD
[0.1.1]: https://github.com/0xdea/zed-highlight/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/0xdea/zed-highlight/releases/tag/v0.1.0

# zed-highlight

[![](https://img.shields.io/github/stars/0xdea/zed-highlight.svg?style=flat&color=yellow)](https://github.com/0xdea/zed-highlight)
[![](https://img.shields.io/crates/v/zed-highlight-lsp?style=flat&color=green)](https://crates.io/crates/zed-highlight-lsp)
[![](https://img.shields.io/crates/d/zed-highlight-lsp?style=flat&color=red)](https://crates.io/crates/zed-highlight-lsp)
[![](https://img.shields.io/badge/zed-1.1.6-violet)](https://zed.dev/)
[![](https://img.shields.io/badge/twitter-%400xdea-blue.svg)](https://twitter.com/0xdea)
[![](https://img.shields.io/badge/mastodon-%40raptor-purple.svg)](https://infosec.exchange/@raptor)
[![build](https://github.com/0xdea/zed-highlight/actions/workflows/build.yml/badge.svg)](https://github.com/0xdea/zed-highlight/actions/workflows/build.yml)

> "Free as in use-after."
>
> -- [@catsalad@infosec.exchange](https://infosec.exchange/@catsalad)

Zed Highlight is a Language Server Protocol (LSP) extension for the [Zed](https://zed.dev/) editor, designed to provide word highlighting. It is useful for quickly identifying all occurrences of selected words in the code, enhancing readability and navigation when tracing the program flow from input sources to potential vulnerability sinks.

![](https://raw.githubusercontent.com/0xdea/zed-highlight/master/.img/screen01.png)

## Features

- Easy access to the following code actions via the `editor: toggle code actions` menu (⌘. shortcut or lightning bolt icon in the gutter):
  - `Highlight: <word>` or `Remove highlight: <word>` - toggle highlighting on and off for the current selection.
  - `Clear all highlights` - remove all highlights with a single command.
- Configurable highlight colors (via `settings.json`).

## See also

- <https://zed.dev/extensions>
- <https://marketplace.visualstudio.com/items?itemName=debugpig.highlight>
- <https://github.com/debugpig/vscode-extension-highlight>
- <https://github.com/huacnlee/color-lsp>

## Installing

TODO - add instructions for installing via Zed's extension marketplace once it's published there.

TODO - add instructions for installing the LSP server manually via `cargo install zed-highlight-lsp` + dev extension

## Usage

TODO - add detailed instructions for using the extension, including custom keymap entries and configuration options.

TODO - add instructions for configuring colors (e.g., via `settings.json`).

Run zed-highlight as follows:

```sh
TODO
```

## Compatibility

The latest release was tested with Zed 1.1.6 on:

- Apple macOS Tahoe 26.4.1
- Ubuntu Linux 24.04.4 LTS - TODO
- Microsoft Windows 11 23H2 - TODO

## Credits

- [@debugpig](https://github.com/debugpig) for their useful [vscode-extension-highlight](https://github.com/debugpig/vscode-extension-highlight), which served as a major inspiration for this project.

## Changelog

- [CHANGELOG.md](https://github.com/0xdea/zed-highlight/blob/master/CHANGELOG.md)

## TODO

- Test with both light and dark themes. Then, release v0.1.0 to the Zed marketplace and [crates.io](https://crates.io/).
- Try another approach to implementing colors via LSP (e.g., `textDocument/documentColor` capability).
- Add command-line interface for manual use of the LSP server outside of Zed.
- Add customizable settings (e.g., `whole_word` and `ignore_case` flags that are already suppported by the LSP).
- Add highlighting based on regular expressions.
- Add sidebar for navigation between currently highlighted words.

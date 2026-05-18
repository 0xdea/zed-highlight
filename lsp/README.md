# zed-highlight-lsp

[![](https://img.shields.io/github/stars/0xdea/zed-highlight.svg?style=flat&color=yellow)](https://github.com/0xdea/zed-highlight)
[![](https://img.shields.io/crates/v/zed-highlight-lsp?style=flat&color=green)](https://crates.io/crates/zed-highlight-lsp)
[![](https://img.shields.io/crates/d/zed-highlight-lsp?style=flat&color=red)](https://crates.io/crates/zed-highlight-lsp)
[![](https://img.shields.io/badge/zed-1.2.3-violet)](https://zed.dev/)
[![](https://img.shields.io/badge/twitter-%400xdea-blue.svg)](https://twitter.com/0xdea)
[![](https://img.shields.io/badge/mastodon-%40raptor-purple.svg)](https://infosec.exchange/@raptor)
[![check](https://github.com/0xdea/zed-highlight/actions/workflows/check.yml/badge.svg)](https://github.com/0xdea/zed-highlight/actions/workflows/check.yml)
[![release](https://github.com/0xdea/zed-highlight/actions/workflows/release.yml/badge.svg)](https://github.com/0xdea/zed-highlight/actions/workflows/release.yml)
[![extension](https://github.com/0xdea/zed-highlight/actions/workflows/extension.yml/badge.svg)](https://github.com/0xdea/zed-highlight/actions/workflows/extension.yml)

> "Free as in use-after."
>
> -- [@catsalad@infosec.exchange](https://infosec.exchange/@catsalad)

Highlight LSP is a Language Server implemented for the [Zed editor](https://zed.dev/), designed to provide word highlighting via the [Highlight](https://github.com/0xdea/zed-highlight) extension.

![](https://raw.githubusercontent.com/0xdea/zed-highlight/master/.img/screen01.png)

## See also

- <https://zed.dev/extensions>
- <https://marketplace.visualstudio.com/items?itemName=debugpig.highlight>
- <https://github.com/debugpig/vscode-extension-highlight>
- <https://github.com/huacnlee/color-lsp>

## Installing

For regular use, Highlight LSP is installed automatically by the [Highlight](https://zed.dev/extensions/html/highlight) extension.

If you want to install it manually, you can do so via [crates.io](https://crates.io/crates/zed-highlight-lsp):

```sh
cargo install zed-highlight-lsp
```

Alternatively, you can build it from [source](https://github.com/0xdea/zed-highlight):

```sh
git clone https://github.com/0xdea/zed-highlight
cd zed-highlight
cargo install --path lsp
```

## Usage

The LSP server is automatically launched by the [Highlight](https://zed.dev/extensions/html/highlight) extension when you open a supported document in Zed. Refer to the [main README](https://github.com/0xdea/zed-highlight/blob/master/README.md) for full usage instructions.

## Compatibility

The latest release was tested with Zed 1.2.3 on:

- Apple macOS Tahoe 26.4.1

## Credits

- [@debugpig](https://github.com/debugpig) for their [vscode-extension-highlight](https://github.com/debugpig/vscode-extension-highlight), which served as a major inspiration for this project.

## Changelog

- [CHANGELOG.md](https://github.com/0xdea/zed-highlight/blob/master/CHANGELOG.md)

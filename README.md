# zed-highlight

[![](https://img.shields.io/github/stars/0xdea/zed-highlight.svg?style=flat&color=yellow)](https://github.com/0xdea/zed-highlight)
[![](https://img.shields.io/crates/v/zed-highlight?style=flat&color=green)](https://crates.io/crates/zed-highlight)
[![](https://img.shields.io/crates/d/zed-highlight?style=flat&color=red)](https://crates.io/crates/zed-highlight)
[![](https://img.shields.io/badge/zed-1.0.1-violet)](https://zed.dev/)
[![](https://img.shields.io/badge/twitter-%400xdea-blue.svg)](https://twitter.com/0xdea)
[![](https://img.shields.io/badge/mastodon-%40raptor-purple.svg)](https://infosec.exchange/@raptor)
[![build](https://github.com/0xdea/zed-highlight/actions/workflows/build.yml/badge.svg)](https://github.com/0xdea/zed-highlight/actions/workflows/build.yml)

> "Free as in use-after."
>
> -- [@catsalad@infosec.exchange](https://infosec.exchange/@catsalad)

Zed Highlight is a Language Server Protocol (LSP) extension for the [Zed](https://zed.dev/) editor, designed to provide word highlighting. It is useful for quickly identifying all occurrences of selected words in the code, enhancing readability and navigation when tracing the program flow from input sources to potential vulnerability sinks.

![](https://raw.githubusercontent.com/0xdea/zed-highlight/master/.img/screen01.png)

## Features

TODO

- Toggle highlighting on and off for a selected word.
- Remove all highlights with a single command.
- Configurable highlight colors.

## Blog post

- TODO

## See also

- <https://zed.dev/extensions>
- <https://marketplace.visualstudio.com/items?itemName=debugpig.highlight>
- <https://github.com/debugpig/vscode-extension-highlight>
- <https://github.com/rsbondi/highlight-words>

## Installing

TODO

The easiest way to get the latest release is via [crates.io](https://crates.io/crates/zed-highlight):

```sh
cargo install zed-highlight
```

To install as a library, run the following command in your project directory:

```sh
cargo add zed-highlight
```

## Compiling

TODO

Alternatively, you can build from [source](https://github.com/0xdea/zed-highlight):

```sh
git clone https://github.com/0xdea/zed-highlight
cd zed-highlight
cargo build --release
```

## Usage

TODO

Run zed-highlight as follows:

```sh
TODO
```

## Compatibility

Tested with Zed 1.0.1 on:

- Apple macOS Tahoe 26.4.1
- Ubuntu Linux 24.04.4 LTS - TODO
- Microsoft Windows 11 23H2 - TODO

## Credits

- [@debugpig](https://github.com/debugpig) for their useful [vscode-extension-highlight](https://github.com/debugpig/vscode-extension-highlight), which served as a major inspiration for this project.

## Changelog

- [CHANGELOG.md](https://github.com/0xdea/zed-highlight/blob/master/CHANGELOG.md)

## TODO

TODO

- Extensively test with both light and dark themes.
- Add customizable settings (e.g., `whole_word`, `ignore_case`).
- Add highlighting based on regular expressions.
- Add sidebar for navigation between highlighted words.

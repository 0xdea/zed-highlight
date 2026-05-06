# zed-highlight-lsp

> "Free as in use-after."
>
> -- [@catsalad@infosec.exchange](https://infosec.exchange/@catsalad)

Zed Highlight LSP is a Language Server implemented for the [Zed](https://zed.dev/) editor, designed to provide word highlighting via my [Zed Highlight](https://github.com/0xdea/zed-highlight) extension.

![](https://raw.githubusercontent.com/0xdea/zed-highlight/master/.img/screen01.png)

## Features

TODO

- Toggle highlighting on and off for a selected word.
- Remove all highlights with a single command.
- Configurable highlight colors.

## Blog post

- TODO

## See also

- <https://github.com/0xdea/zed-highlight>
- <https://zed.dev/extensions>
- <https://marketplace.visualstudio.com/items?itemName=debugpig.highlight>
- <https://github.com/debugpig/vscode-extension-highlight>
- <https://github.com/rsbondi/highlight-words>

## Installing

TODO

`cargo install --path lsp`

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

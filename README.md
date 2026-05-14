# zed-highlight

[![](https://img.shields.io/github/stars/0xdea/zed-highlight.svg?style=flat&color=yellow)](https://github.com/0xdea/zed-highlight)
[![](https://img.shields.io/crates/v/zed-highlight-lsp?style=flat&color=green)](https://crates.io/crates/zed-highlight-lsp)
[![](https://img.shields.io/crates/d/zed-highlight-lsp?style=flat&color=red)](https://crates.io/crates/zed-highlight-lsp)
[![](https://img.shields.io/badge/zed-1.2.3-violet)](https://zed.dev/)
[![](https://img.shields.io/badge/twitter-%400xdea-blue.svg)](https://twitter.com/0xdea)
[![](https://img.shields.io/badge/mastodon-%40raptor-purple.svg)](https://infosec.exchange/@raptor)
[![build](https://github.com/0xdea/zed-highlight/actions/workflows/build.yml/badge.svg)](https://github.com/0xdea/zed-highlight/actions/workflows/build.yml)

> "Free as in use-after."
>
> -- [@catsalad@infosec.exchange](https://infosec.exchange/@catsalad)

Zed Highlight is a Language Server Protocol (LSP) extension for the [Zed editor](https://zed.dev/), designed to provide word highlighting. It is useful for quickly identifying all occurrences of selected words in the code, enhancing readability and navigation when tracing the program flow from input sources to potential vulnerability sinks.

![](https://raw.githubusercontent.com/0xdea/zed-highlight/master/.img/screen01.png)

## Features

The following features are currently supported by the extension and the bundled [LSP server](https://github.com/0xdea/zed-highlight/tree/master/lsp):

- Easy access to the following code actions via the `editor: toggle code actions` menu (`⌘.` shortcut or lightning bolt icon in the gutter):
  - `Highlight` or `Remove highlight` - Toggle highlighting on and off for the current selection.
  - `Clear all highlights` - Remove all highlights with a single command.
- Configurable highlight colors (via `settings.json`).

## See also

- <https://zed.dev/extensions>
- <https://marketplace.visualstudio.com/items?itemName=debugpig.highlight>
- <https://github.com/debugpig/vscode-extension-highlight>
- <https://github.com/huacnlee/color-lsp>

## Installing

The easiest way to install the Zed Highlight extension is via Zed's [extension marketplace](https://zed.dev/extensions/html/zed-highlight). It will take care of installing the latest release of the bundled [LSP server](https://github.com/0xdea/zed-highlight/tree/master/lsp) automatically.

Alternatively, you can clone the repository and install both the extension and the LSP server manually. First, build the LSP server from source:

```sh
git clone https://github.com/0xdea/zed-highlight
cd zed-highlight
cargo install --path lsp
```

Then, in Zed, run `zed: install dev extension` from the command palette and select the `zed-highlight` directory in which you have cloned the repository.

## Configuration

Make sure [syntax highlighting with semantic tokens](https://zed.dev/docs/extensions/languages#syntax-highlighting-with-semantic-tokens) is enabled in `settings.json`, with e.g.:

```json
{
  // enable combined semantic tokens (recommended)
  "semantic_tokens": "combined"
}
```

or:

```json
{
  // enable full semantic tokens
  "semantic_tokens": "full"
}
```

Then, configure the colors for the semantic tokens provided by the extension (e.g., `zed-highlight-0` to `zed-highlight-7`) in `settings.json`. For example, you can use the following color scheme for dark themes:

```json
{
  // zed-highlight extension colors (dark themes)
  "global_lsp_settings": {
    "semantic_token_rules": [
      {
        "token_type": "zed-highlight-0",
        "foreground_color": "#F5B041",
        "background_color": "#F5B04150"
      },
      {
        "token_type": "zed-highlight-1",
        "foreground_color": "#85C1E9",
        "background_color": "#85C1E950"
      },
      {
        "token_type": "zed-highlight-2",
        "foreground_color": "#CD6155",
        "background_color": "#CD615550"
      },
      {
        "token_type": "zed-highlight-3",
        "foreground_color": "#AF7AC5",
        "background_color": "#AF7AC550"
      },
      {
        "token_type": "zed-highlight-4",
        "foreground_color": "#48C9B0",
        "background_color": "#48C9B050"
      },
      {
        "token_type": "zed-highlight-5",
        "foreground_color": "#F4D03F",
        "background_color": "#F4D03F50"
      },
      {
        "token_type": "zed-highlight-6",
        "foreground_color": "#52BE80",
        "background_color": "#52BE8050"
      },
      {
        "token_type": "zed-highlight-7",
        "foreground_color": "#FF9933",
        "background_color": "#FF993350"
      }
    ]
  }
}
```

An alternative color scheme that should be more suitable for light themes is also provided below:

```json
{
  // zed-highlight extension colors (light themes)
  "global_lsp_settings": {
    "semantic_token_rules": [
      {
        "token_type": "zed-highlight-0",
        "foreground_color": "#B3D9FF",
        "background_color": "#B3D9FF50"
      },
      {
        "token_type": "zed-highlight-1",
        "foreground_color": "#B3B3FF",
        "background_color": "#B3B3FF50"
      },
      {
        "token_type": "zed-highlight-2",
        "foreground_color": "#FFD9B3",
        "background_color": "#FFD9B350"
      },
      {
        "token_type": "zed-highlight-3",
        "foreground_color": "#FFB3FF",
        "background_color": "#FFB3FF50"
      },
      {
        "token_type": "zed-highlight-4",
        "foreground_color": "#B3FFB3",
        "background_color": "#B3FFB350"
      },
      {
        "token_type": "zed-highlight-5",
        "foreground_color": "#D1E0E0",
        "background_color": "#D1E0E050"
      },
      {
        "token_type": "zed-highlight-6",
        "foreground_color": "#FFFF80",
        "background_color": "#FFFF8050"
      },
      {
        "token_type": "zed-highlight-7",
        "foreground_color": "#E6FFB3",
        "background_color": "#E6FFB350"
      }
    ]
  }
}
```

## Usage

Use the Zed Highlight extension as follows:

- TODO

## Compatibility

The latest release was tested with Zed 1.2.3 on:

- Apple macOS Tahoe 26.4.1

## Credits

- [@debugpig](https://github.com/debugpig) for their useful [vscode-extension-highlight](https://github.com/debugpig/vscode-extension-highlight), which served as a major inspiration for this project.

## Changelog

- [CHANGELOG.md](https://github.com/0xdea/zed-highlight/blob/master/CHANGELOG.md)

## TODO

- Test with both light and dark themes. Then, release to the Zed marketplace and [crates.io](https://crates.io/).
- Try another approach to implementing colors via LSP (e.g., the `textDocument/documentColor` capability).
- Add a command-line interface for manual use of the LSP server outside of Zed.
- Add customizable settings (e.g., `whole_word` and `ignore_case` flags that are already suppported by the LSP).
- Add highlighting based on regular expressions.
- Add a sidebar for navigation between currently highlighted words.

```

```

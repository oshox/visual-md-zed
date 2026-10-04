# Zed MD Sample

A small extension that shows what extensions can add to Zed MD's Markdown live
preview. It is the worked example for `docs/visual-md-extensions.md`.

## What it adds

| Hook                      | Name            | What it does                                                        |
| ------------------------- | --------------- | ------------------------------------------------------------------- |
| Fenced code block         | `sample-flow`   | Draws `a -> b -> c` lines as boxes joined by arrows, as an SVG.     |
| Fenced code block         | `sample-table`  | Turns comma separated rows into a Markdown table.                   |
| Fenced code block         | `sample-styled` | Picks out numbers, SHOUTING words and `#tags` with styled text.     |
| Editor command            | `uppercase`     | Uppercases the selection, or the current line with nothing selected. |

````markdown
```sample-flow
parse -> check -> emit
```
````

While the cursor is outside a block, the extension's rendering stands in for
it. Moving the cursor into the block, or clicking it, shows the source.

Run the command from the command palette (`Uppercase Selection`, listed while
live preview is showing) or bind it in your keymap:

```json
{
  "context": "Editor && visual_md",
  "bindings": {
    "ctrl-alt-u": [
      "visual_md::RunExtensionCommand",
      { "id": "visual-md-sample.uppercase" }
    ]
  }
}
```

## Building and installing

Extensions are WebAssembly, built for `wasm32-wasip2`. If your Rust install
does not have that target, `script/visual-md-wasm setup` installs an isolated
toolchain for it without changing your own.

- `script/visual-md-wasm build-sample` builds the extension, which the tests that
  load real wasm need.
- To try it in the app, run Zed through `script/visual-md-wasm run -- <zed>`
  and use **zed: install dev extension** on this directory, so that the app can
  compile it. Where the target is installed system-wide, no wrapper is needed.

The pure parts are ordinary unit tests: `cargo test -p visual_md_sample`.

# Zed MD Sample

A small extension that shows what extensions can add to Zed MD's Markdown live
preview. It is the worked example for `docs/visual-md-extensions.md`.

## What it adds

| Hook                      | Name            | What it does                                                        |
| ------------------------- | --------------- | ------------------------------------------------------------------- |
| Fenced code block         | `sample-flow`   | Draws `a -> b -> c` lines as boxes joined by arrows, as an SVG.     |
| Fenced code block         | `sample-table`  | Turns comma separated rows into a Markdown table.                   |
| Fenced code block         | `sample-styled` | Picks out numbers, SHOUTING words and `#tags` with styled text.     |
| Syntax rule               | `mention`       | Styles `@names` in blue and bold. No code needed.                   |
| Dynamic syntax rule       | `emoji`         | Shows `:smile:`, `:tada:` and a few more as emoji.                  |
| Callout                   | `sample`        | `> [!sample]` as a purple callout titled "Sample" with its own icon. |
| Editor command            | `uppercase`     | Uppercases the selection, or the current line with nothing selected. Also offered as `/uppercase`. |
| Document events           | `opened`, `saved`, `changed` | Appends a line about the document to `events.log` in the extension's work directory. |
| Link scheme               | `sample`        | `[text](sample://docs)` opens `https://example.com/sample/docs`.    |
| Wikilink completions      | `[[`            | Suggests the names in the `notes` setting and the project's Markdown files. |

````markdown
```sample-flow
parse -> check -> emit
```
````

While the cursor is outside a block, the extension's rendering stands in for
it. Moving the cursor into the block, or clicking it, shows the source.

Type `/` at the start of a line and `Uppercase Selection` is offered in the
completion menu. Type `[[` and the names it knows are offered, the project's
Markdown files by name and these, which are set in `settings.json`:

```json
{
  "visual_md": {
    "extensions": {
      "visual-md-sample": { "notes": ["Ideas", "Inbox"] }
    }
  }
}
```

Hold Cmd (Ctrl on Linux and Windows) over `[a link](sample://docs)` to see it
become clickable. The events are in `events.log` under the extension's work
directory, in `work/visual-md-sample` of Zed's extensions directory, one line for
each: `saved /notes/a.md: 2 headings, 1 links, 0 tags, 3 tasks`.

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

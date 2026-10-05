# Writing a Zed MD extension

Extensions can hook into Zed MD's Markdown live preview. They are ordinary Zed
extensions: WebAssembly components built on `zed_extension_api`, installed like
any other. This guide covers what exists today. The milestone it belongs to,
M15 in [`visual-md-milestones.md`](./visual-md-milestones.md), is being built in
stages, and each stage adds its hooks here.

| Hook                           | Status    |
| ------------------------------ | --------- |
| Fenced code block renderers    | Available |
| Editor commands                | Available |
| Syntax rules and callouts      | Planned   |
| Events, outline, link provider | Planned   |
| `/` and `[[` completions       | Planned   |
| Extension settings             | Planned   |

A working example is in [`extensions/visual-md-sample`](../extensions/visual-md-sample).

## Versioning and caveats

The hooks need extension API version **0.9.0**, which exists only in this fork.
Upstream Zed has never released it, so:

- An extension built for 0.9.0 loads on every release channel here. Version
  0.8.0 is still limited to development builds, as upstream has it.
- Zed MD never advertises 0.9.0 to the extension registry, and the registry
  has no extensions for it. Install yours as a dev extension.
- 0.9.0 is **unstable until M15 is finished**. Each stage adds exports to it, and
  WebAssembly components are checked structurally, so an extension has to be
  rebuilt against each stage.
- Extensions can make network requests without declaring it, exactly as they
  can in upstream Zed. Process execution and file downloads still need the
  capabilities an extension declares. Review an extension's code before
  installing it.

## A first extension

```
my-extension/
  extension.toml
  Cargo.toml
  src/my_extension.rs
```

`extension.toml`:

```toml
id = "my-extension"
name = "My Extension"
description = "Renders flow blocks and uppercases text."
version = "0.1.0"
schema_version = 1
authors = ["Me"]
repository = "https://example.com/me/my-extension"

[visual_md]
fence_renderers = ["flow"]

[visual_md.commands.uppercase]
title = "Uppercase Selection"
description = "Uppercases the selected text."
```

`Cargo.toml`:

```toml
[package]
name = "my_extension"
version = "0.1.0"
edition = "2024"

[lib]
path = "src/my_extension.rs"
crate-type = ["cdylib"]

[dependencies]
zed_extension_api = { path = "path/to/zed/crates/extension_api" }
```

`zed_extension_api` is a path dependency because 0.9.0 is not published.

`src/my_extension.rs`:

```rust
use zed_extension_api::{self as zed, visual_md};

struct MyExtension;

impl zed::Extension for MyExtension {
    fn new() -> Self {
        Self
    }

    fn visual_md_render_fence(
        &self,
        renderer: String,
        request: visual_md::FenceRequest,
    ) -> Result<visual_md::FenceResult, String> {
        Ok(visual_md::FenceResult {
            output: visual_md::FenceOutput::Markdown(format!(
                "**{renderer}** block with {} lines",
                request.content.lines().count()
            )),
            height_hint: Some(2),
        })
    }

    fn visual_md_run_command(
        &self,
        command: String,
        context: visual_md::CommandContext,
    ) -> Result<visual_md::CommandResult, String> {
        // See "Editor commands" below.
        Err(format!("no command `{command}`"))
    }
}

zed::register_extension!(MyExtension);
```

Both methods have defaults that return an error, so implement only the hooks
the manifest declares.

## The manifest

Everything lives under `[visual_md]` in `extension.toml`.

| Key | Meaning |
| --- | --- |
| `fence_renderers` | Language tags of the fenced code blocks the extension renders. Any case. |
| `[visual_md.commands.<id>]` | An editor command. `title` is required, `description` is optional. |

Command ids may contain letters, digits, `-` and `_`. A command is run as
`<extension id>.<command id>`. An invalid `[visual_md]` section is logged and the
extension's hooks are not registered at all. If two extensions claim the same
language, the one whose id sorts first renders it.

A `[visual_md]` section in an extension with no wasm registers its
declarations but has nothing to run, so a declared renderer or command reports
that it has no code. Later stages add declarations that need no code.

## Fenced code block renderers

A block like

````markdown
```flow
parse -> check -> emit
```
````

is replaced by the extension's output whenever no selection touches it. Putting
the cursor anywhere in the block, or clicking it, shows the source again with the
usual fence borders and code highlighting; moving away renders it again. A fence
that is never closed is not rendered. Removing or disabling the extension
restores the default rendering.

```wit
export visual-md-render-fence: func(
    renderer: string,
    request: fence-request,
) -> result<fence-result, string>;
```

`renderer` is the language tag as the manifest declared it. The request has:

- `language`: the tag, lowercased.
- `info`: the whole info string after the opening fence, such as
  `flow {width=3}`.
- `content`: the text between the fences, exactly as written.
- `appearance`: `light` or `dark`, for the active theme.
- `path`: the file's path, if it has one.

The result has an `output` and an optional `height-hint`, the number of editor
rows the output should take. It only sizes the block until the block has been
laid out. The output is one of:

| Output        | Shown as                                                |
| ------------- | ------------------------------------------------------- |
| `styled-text` | Text in the code font with styles on byte ranges of it. |
| `markdown`    | Markdown, drawn with the editor's Markdown renderer.    |
| `svg`         | An SVG document.                                        |
| `image`       | An encoded PNG, JPEG, GIF or WebP.                      |

Styles have optional hex `color` and `background-color` (`#rgb`, `#rgba`,
`#rrggbb` or `#rrggbbaa`; one that does not parse is ignored), a `theme-token`
such as `keyword` or `number` to take colors from the active theme (explicit
colors win), `font-weight` from 100 to 900, and `italic`, `underline` and
`strikethrough`.

Return an `Err` message to show the user what went wrong in place of the block;
it is shown after the text "Could not render this block:".

Results are cached by extension build, language, info string, content, theme
appearance and file path, and shared between editors, so a block is only
rendered again when one of those changes. Rebuilding the extension counts as a
new build.

## Editor commands

```wit
export visual-md-run-command: func(
    command: string,
    context: command-context,
) -> result<command-result, string>;
```

`command` is the id from the manifest, without the extension id. The context has
the whole document `text`, the `selections` as byte ranges (empty ranges are
cursors), and the `path`. The result has:

- `edits`: replacements of byte ranges of the text the command ran on. All of
  them are in the coordinates of that original text, not of the text after the
  earlier edits.
- `selections`: optional ranges to select afterwards, in the coordinates of the
  text **after** the edits. Ranges that do not fit are ignored, and the editor
  keeps its own selections if none do.
- `message`: optional text shown to the user as a notification.

The edits are checked before any is applied: each must lie inside the document
on character boundaries, none may overlap another, and there may be at most
100,000 of them inserting at most 16 MB. If any check fails, nothing changes and
the user is told. Valid edits are applied as one undo step. If the document
changed while the command ran, the result is dropped, and the user is told, so
that it is never applied to the wrong place.

Run a command from the command palette, where it is listed as
`<extension name>: <title>` while live preview is showing in the active editor,
or bind it in a keymap:

```json
{
  "context": "Editor && visual_md",
  "bindings": {
    "ctrl-alt-u": [
      "visual_md::RunExtensionCommand",
      { "id": "my-extension.uppercase" }
    ]
  }
}
```

## Limits

An extension runs in the same process as Zed, one call at a time, so Zed guards
every call into it:

| Limit | Value |
| --- | --- |
| Time for a fence render | 5 seconds |
| Time for a command | 10 seconds |
| Calls in flight per extension | 4. More wait their turn. |
| Text output | 1 MB for Markdown, SVG and styled text |
| Image output | 16 MB |
| Failures in a row | After 3 failed or timed out calls, the extension's Markdown hooks are disabled until Zed restarts or the extension is reloaded. |

A call that times out is abandoned, but it cannot be stopped: the extension keeps
running it and cannot answer anything else until it finishes. Keep calls quick.
Styled spans that are out of bounds, empty, off a character boundary or
overlapping an earlier span are dropped, and the rest of the output is used.

None of this runs while Zed is drawing or while you type. A block shows
"Rendering…" until its result arrives.

## Building and installing

Extensions are built for `wasm32-wasip2`. If your Rust install has that target,
build and install as for any Zed extension. If it does not,
`script/visual-md-wasm` sets up an isolated toolchain for it, without changing
yours:

```sh
script/visual-md-wasm setup          # once
script/visual-md-wasm build-sample   # builds extensions/visual-md-sample
script/visual-md-wasm run -- target/debug/zedmd
```

`run` puts that toolchain first on `PATH` for the one command. Start Zed that way
and use **zed: install dev extension** on your extension's directory so that Zed
can compile it. Installing it again after a change registers the new build and
refreshes open Markdown editors.

The sample's pure parts are plain unit tests, and
`test_visual_md_sample_extension` in `crates/extension_host` loads the built
sample through the host and calls every hook.

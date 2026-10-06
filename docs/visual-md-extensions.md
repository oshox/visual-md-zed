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
| Syntax rules and callouts      | Available |
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
| `[[visual_md.syntax_rules]]` | A rule that styles, hides or replaces text. See below. |
| `[visual_md.callouts.<name>]` | A callout type, written `> [!name]`. See below. |

Command ids may contain letters, digits, `-` and `_`. A command is run as
`<extension id>.<command id>`. An invalid `[visual_md]` section is logged and the
extension's hooks are not registered at all. If two extensions claim the same
language, the one whose id sorts first renders it.

A `[visual_md]` section in an extension with no wasm registers its
declarations but has nothing to run, so a declared renderer or command reports
that it has no code. Syntax rules without `dynamic = true`, and callouts, need
no code, so an extension can be only a manifest.

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

## Syntax rules

A syntax rule styles, hides or replaces text that matches a pattern.

```toml
[[visual_md.syntax_rules]]
id = "mention"
pattern = '@\w+'
style = { color = "#3b82f6", font_weight = 600 }

[[visual_md.syntax_rules]]
id = "emoji"
pattern = '(:)([a-z_]+)(:)'
dynamic = true
```

| Key | Meaning |
| --- | --- |
| `id` | Names the rule. Letters, digits, `-` and `_`, unique in the extension. |
| `pattern` | A regular expression in the syntax of Rust's `regex` crate. |
| `node` | Instead of `pattern`, a kind of syntax node to match. See below. |
| `style` | How to style every match: `color` and `background_color` as hex, `theme_token`, `font_weight`, `italic`, `underline`, `strikethrough`. |
| `hide` | Capture groups of `pattern` to hide. Group 0 is the whole match. |
| `dynamic` | Ask the extension what to do with each match. See below. |

A rule has exactly one of `pattern` and `node`, and must do something: it needs
a `style`, a `hide` or `dynamic = true`. A pattern must not match the empty
string, and a pattern too large to compile, or one that hides a group it does
not have, is logged and the rule left out.

A `pattern` is matched against the text of paragraphs, headings, list items,
quotes and table cells, outside inline code. A match never spans a line break.
A `node` is one of `html_tag`, `full_reference_link`, `collapsed_reference_link`,
`shortcut_link`, `html_block`, `link_reference_definition`, `minus_metadata` (a
`---` front matter block) or `plus_metadata` (`+++`). They are the constructs
Zed MD's Markdown grammar produces and Zed MD does not itself decorate.

Hidden text comes back, dimmed, while a selection touches the match, so that it
can be edited, and the match shows its source again. A style stays. An
extension's style is drawn over Zed MD's own where it sets the same thing.

Zed MD's own decorations win. Text an extension asks to hide or replace is left
alone when it overlaps anything Zed MD folds or reveals itself: its hidden
markers, bullets, checkboxes, callout titles, table pipes, spacing and
delimiter rows, rules, images and fenced blocks. Where two extensions' ranges
overlap, the leftmost wins.

### Dynamic rules

With `dynamic = true` the extension is asked about the matches, on top of the
rule's `style` and `hide`:

```wit
export visual-md-apply-rule: func(
    rule: string,
    matches: list<rule-match>,
) -> result<list<rule-output>, string>;
```

A `rule-match` has the matched `text` and the range of each capture group
within it, group 0 first (`none` for a group that took no part). Answer with one
`rule-output` per match, in order: `spans` (styles), `hidden` ranges and
`replacements` (a range and the text to show in its place). Every range is in
the coordinates of the match's text, so one answer serves the same text wherever
it appears; it is filed under the extension build, the rule and the text, and
the first place a text was seen supplies the capture groups.

Until the answer arrives the match shows as written; the answer is applied
when it lands. Hidden ranges and replacements are only applied while no
selection touches the match. Whatever cannot be used is dropped and the rest
kept: ranges that are empty, out of bounds, off a character boundary or
overlapping an earlier one, hidden ranges and replacements with a line break,
replacements over 200 characters, and replacements that overlap a hidden range.
A call that fails, times out or returns the wrong number of outputs leaves all
its matches as written, and they are not asked about again until the extension
is reloaded.

## Callouts

```toml
[visual_md.callouts.todo]
title = "To do"
kind = "tip"
icon = "icons/todo.svg"
accent = "#a855f7"
background = "#a855f71a"
```

`> [!todo]` then shows as a callout box like the built-in ones. Names ignore
case, and when two extensions register one, the one whose id sorts first wins.
Every key is optional.

- `title` is shown instead of the capitalized name.
- `kind` is the built-in callout a type starts from: `note`, `tip`, `warning`
  or `danger`, or an alias such as `info`, `success`, `caution` or `error`. It
  decides the defaults for what is not set. It replaces the generic kind of a
  name Zed MD does not know, and never changes a built-in name's kind. An
  unknown kind is logged and the callout left out.
- `icon` is the name of one of Zed's icons, such as `star`, or the path of an
  `.svg` file inside the extension. An unknown icon name falls back to the
  kind's icon.
- `accent` and `background` are hex colors.

What the user sets for the name in `visual_md.callouts` or
`visual_md.colors.callout`, and what the theme sets in a
`visual_md.callout.<name>` token, still win over the extension. The extension
wins over what is set for the kind and over the kind's defaults. SVG files
declared as icons are packaged with the extension.

## Limits

An extension runs in the same process as Zed, one call at a time, so Zed guards
every call into it:

| Limit | Value |
| --- | --- |
| Time for a fence render | 5 seconds |
| Time for a command | 10 seconds |
| Time for a dynamic rule call | 1 second |
| Matches in one dynamic rule call | 256, each distinct text once |
| Replacement text | 200 characters, one line |
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

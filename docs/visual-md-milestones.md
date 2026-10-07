# Zed MD markdown editor milestones

The live-preview markdown editor lives in the `visual_md` crate and is
configured under the `visual_md` settings key. The names stay `visual_md` on
purpose even though the app is branded Zed MD.

## Status

| Milestone | Feature | State |
| --- | --- | --- |
| M1 | Inline formatting (headings, bold, italic, strike, highlight, code) | Done |
| M2 | Lists, tasks, blockquotes | Done |
| M3 | Viewport-only decoration, copy/paste, on/off setting | Done |
| M4 | Taller heading rows | Done |
| M5 | Smart list continuation | Done |
| M6 | Links, autolinks, horizontal rules | Done |
| M7 | Proportional prose font, monospace code | Done |
| M8 | Fenced code blocks with syntax highlighting | Done |
| M9 | GFM tables | Done |
| M10 | Bold and italic shortcuts | Done |
| M11 | Callout boxes | Done |
| M12 | Inline images | Done |
| M13 | Turn the editor off globally or per project, in settings.json and the settings UI | Done |
| M14 | Full font and color customization, in settings.json, the settings UI and themes | Done |
| M15 | Extension hooks on par with Obsidian, Notion and Logseq | Built. The hooks that need keystrokes or the mouse were not run in the app, see "Checked in the running app" under M15 |
| M16 | Block layout fixes, native wikilinks, tags, comments, block ids and task marks | Built. Clicking a link or a tag was not run in the app, see "As built" under M16 |
| M17 | Editing tools and the outliner | In progress: editing tools and folding built, completion planned, see "M17" |
| M18 | Block syntax: properties panel, footnotes, math, embeds, hover previews | Planned, see the feature audit |
| M19 | Extension surface for knowledge-base features | Planned, see the feature audit |

## M13: On/off switch at global and project level

**Today:** `"visual_md": { "enabled": true }` exists, but only in the user
settings file. `visual_md` is not part of `ProjectSettingsContent`, the crate
reads it with `VisualMdSettings::try_get(cx)` (no file location), and the
settings UI has no entry for it.

**Deliverables**
1. `.zed/settings.json` in a project can set `visual_md.enabled`, and it wins
   over the user setting for files in that worktree. This means moving the
   `visual_md` field into the part of the settings content that project files
   merge, and reading it with the buffer's `SettingsLocation` everywhere the
   crate checks `enabled` (decoration refresh, the Enter handler, the
   bold/italic shortcuts, the `visual_md` key context).
2. Per-language override as a bonus of the same change: `enabled` can also be
   set under `languages`, for example
   `"languages": { "Markdown": { "visual_md": { "enabled": false } } }`.
   The live preview only runs on buffers whose language is named `Markdown`,
   so other language entries have no effect until the crate learns to treat
   another language (such as an MDX extension's) as Markdown.
3. A "Markdown Live Preview" section in the settings UI with an "Enabled"
   toggle. The settings UI already switches between User and project scopes,
   so the same toggle covers both levels.
4. A command palette action, `visual_md: toggle live preview`, that flips the
   setting for the current editor only, without writing any file. Useful for
   a quick look at the raw source. The override belongs to that editor, so a
   second split of the same file is unaffected, and it is dropped once the
   settings change to agree with it.
5. Flipping any of these while a file is open re-renders it immediately with
   no leftover folds or highlights (the current off path already clears them;
   this adds tests for the project and per-buffer paths).

**Done when:** a project setting of `false` turns the editor off only in that
project, the settings UI toggle writes the right file for the chosen scope,
and tests cover user, project, language and per-buffer precedence.

## M14: Full font and color customization

**Today:** every font and color is hard-wired. Prose uses the UI font, code
uses the buffer font, heading sizes are fixed multipliers, links use the
theme's `link_text_hover`, and callouts reuse the theme's status colors. The
M11 and M1 code comments record an earlier choice to add no extra tints to
inline code, highlights or callout bodies. M14 keeps those defaults but lets
the user change them.

**Deliverables**
1. **Fonts in settings.json:** new keys under `visual_md`:
   `prose_font_family`, `prose_font_size`, `prose_font_weight`,
   `prose_line_height`, `code_font_family`, `code_font_size`, and
   `heading_font_family`. Leaving a key unset keeps today's behavior
   (UI font for prose, buffer font for code).
2. **Heading scale:** `heading_sizes` (six multipliers for H1 to H6) and
   per-level weight. Both are objects with `h1` to `h6` keys rather than
   arrays, because arrays overwrite as a whole when settings merge and the
   settings UI writes one level at a time.
3. **Colors in settings.json:** a `visual_md.colors` object with a key per
   element: `heading` (and `heading.1` to `heading.6`), `bold`, `italic`,
   `strikethrough`, `highlight.background`, `inline_code` and
   `inline_code.background`, `link`, `marker` (the dimmed syntax shown on the
   cursor line), `blockquote.bar`, `rule`, `table.border`, `code_block.border`,
   `code_block.background`, `task.checked`, and `callout.<type>` with
   `accent` and `background` for note, tip, warning, danger and custom types.
4. **Colors in themes:** the same keys are read from a theme's `syntax` map
   as `visual_md.*` tokens, so a theme extension or `theme_overrides` can
   style the editor. Order of precedence: `visual_md.colors` setting, then
   the active theme's `visual_md.*` token, then today's default.
5. **Custom callout types:** `visual_md.callouts` maps a type name (for
   example `"quote"` or `"todo"`) to an icon, accent and background, so
   `> [!todo]` gets its own look instead of the neutral fallback.
6. **Project scope:** all of the above live in the same settings struct as
   M13, so a project's `.zed/settings.json` can override them.
7. **Settings UI:** the "Markdown Live Preview" section gains font pickers,
   size fields, heading scale fields and a color field for each element,
   each with a reset-to-default button.

**Done when:** each key changes the running editor without a restart,
unset keys match today's rendering exactly, and a theme can restyle the
editor with no settings change.

### As built

**Precedence.** For every color: the `visual_md` setting (user, then project,
then `languages.<Name>`), then the active theme's `visual_md.*` token, then
the default live preview has always had. Fonts and sizes come from settings
only, since a theme's `syntax` entries cannot carry them.

**Theme tokens.** A setting key `x` is read from the theme as the `color` of
the `syntax` token `visual_md.x`, and a key `x.background` as the
`background_color` of that same token. So a theme styles inline code with one
token, `visual_md.inline_code`, that has both. `heading.1` to `heading.6` fall
back to `heading`. A callout type is one token too, `visual_md.callout.<type>`:
its `color` is the accent and its `background_color` the background.

**Fonts.**
- `prose_font_family` defaults to the UI font, `code_font_family` to the
  buffer font, and `heading_font_family` to the prose font.
- `prose_font_size` and `prose_line_height` set the editor's own text style, so
  soft wrap measures prose at the right size and the ctrl-scroll zoom keeps
  working. An existing refinement of that style is saved and restored.
- `code_font_size` sizes both inline code and fenced code blocks. Unset, code
  keeps the buffer font size, so changing only the prose size does not change
  code. Inline code is a different size from the prose around it through a
  per-run font size in gpui.
- `prose_font_weight` applies to prose only: code keeps the buffer font's
  weight, and bold and headings keep their own.

**Callout types.** The type written in `> [!name]` is matched case-insensitively.
For the accent, background and icon, the first of these that is set wins:
`callouts.<name>`, `colors.callout.<name>`, `colors.callout.<kind>` where the
kind is `note`, `tip`, `warning`, `danger` or `other` (an alias such as `info`
belongs to `note`), then the theme tokens for the name and the kind, then the
kind's default. An icon is the snake_case name of one of Zed's icons, for
example `check` or `star`; an unknown name keeps the kind's icon.

**Limitations.**
- `code_block.background` tints the code text, so it is ragged at the end of
  each line, the same as a callout body. A full-width tint needs an editor API
  that does not exist yet.
- Kerning, ligatures and bidi reordering do not span a font size change, so a
  line with inline code at another size is shaped in separate pieces.
- Soft wrap measures a line at one size. Heading rows, which were already
  wider than their measured width, and lines with inline code at another size
  can wrap a little early or late.
- A row of smaller text, such as code below the prose size, keeps the height
  of a prose row, so smaller code is not denser.
- The settings UI has no control for custom callout types, since it has no
  editor for maps. Colors are hex text inputs with a swatch, not a picker.

## M15: Extension hooks

**Today:** Zed extensions are WebAssembly components built on
`zed_extension_api` (WIT under `crates/extension_api/wit`). They can add
languages, language servers, themes, slash commands, context servers and
debug adapters. They cannot touch how the markdown editor renders or
behaves.

**What the reference apps offer, and the equivalent here**

| Obsidian / Notion / Logseq | Zed MD equivalent |
| --- | --- |
| Markdown post-processors, custom syntax (Obsidian), macros (Logseq) | Inline and block **syntax rules**: an extension declares a pattern (regex or tree-sitter node kind) and returns styled spans, hidden ranges and replacement text |
| Code block processors (mermaid, dataview, charts) | **Fenced block renderers**: an extension claims a language tag and returns rendered output (styled text, markdown, SVG or an image) shown in place of the block when the cursor is away |
| Custom callout types and icons | **Callout registration** that plugs into M14's callout table |
| Commands, slash menus (Notion `/`), hotkeys | **Editor commands**: an extension registers a named command that receives the buffer text and selections and returns edits; commands appear in the palette, can be bound in keymaps, and can appear in a `/` insert menu |
| Events (file open, save, change), metadata cache | **Document events** (`opened`, `saved`, `changed` with debounce) and a parsed **document outline** (headings, links, tags, tasks, frontmatter) passed to the extension |
| Link resolvers, wikilink completions | **Link providers**: resolve a custom link scheme or wikilink target and supply completions |
| Settings tabs | Extension settings under `visual_md.extensions.<id>` shown in the settings UI |
| Themes and CSS snippets | Covered by M14's `visual_md.*` theme tokens, which existing theme extensions can already ship |

**Deliverables**
1. A new `visual-md.wit` interface in a new `since_v0.9.0` extension API
   version, with the hooks above. Extensions opt in through
   `extension.toml` (for example `[visual_md] syntax_rules = [...]`,
   `fence_renderers = ["mermaid"]`).
2. Host side in the `visual_md` crate: rule results merge into the existing
   render plan, results are cached per buffer version, and a slow or failing
   extension is timed out and logged instead of blocking typing.
3. Sandboxing stays as Zed's today: extensions only get the capabilities
   they declare (for example network for a renderer that calls a web
   service).
4. A sample extension in `extensions/visual-md-sample` that adds a custom
   syntax rule, a `mermaid`-style fence renderer, a custom callout and an
   editor command, used by integration tests.
5. A developer guide in `docs/` for writing a Zed MD extension.

**Not in M15:** arbitrary custom panes or database views like Notion
databases or Obsidian's Dataview tables beyond what a fence renderer can
draw. Zed's extension model has no general UI API, and building one is a
separate, much larger project.

**Suggested order inside M15:** fence renderers and editor commands first
(highest value, smallest surface), then syntax rules, then events, outline
and link providers.

**Done when:** the sample extension installs as a dev extension and each of
its hooks works in the running app, and removing it restores the default
rendering.

### As built

The developer guide, [`visual-md-extensions.md`](./visual-md-extensions.md),
documents every hook, key and limit. This records the decisions behind them.

**API version.** The hooks are extension API 0.9.0, a version that exists only
in this fork: upstream has never released one, and its 0.8.0 is still
development-only. `since_v0_9_0.rs` is a thin layer over the 0.8.0
implementation, so upstream's edits to 0.8.0 do not conflict with it. 0.9.0
loads on every release channel, and the registry is never told about it, so it
cannot collide with the 0.9.0 upstream eventually releases. Its WIT changes
until it is released, and components are checked structurally, so a guest
must be rebuilt against the version of Zed MD it runs in.

**Manifest.** Everything is under `[visual_md]` in `extension.toml`. A section
that does not validate is logged and none of that extension's hooks register.
Syntax rules without `dynamic = true` and callouts need no code, so an
extension can be only a manifest.

**Calls into an extension.** A call never happens while the editor draws or a
key is handled: refresh reads caches and asks for what is missing, and the
answer re-applies when it lands. Every call has a timeout (fence 5 s, command
10 s, dynamic rule 1 s, event 5 s, link 2 s, completion 1 s), at most four
are in flight per extension, and three failures in a row disable an extension's
Markdown hooks until restart or reload. Answers are cached by extension build,
so reinstalling a dev extension never shows stale output.

**Precedence.**
- Zed MD's own decorations win. A range an extension asks to hide or replace is
  dropped when it overlaps anything Zed MD folds or reveals, because two folds
  over the same text would panic the editor.
- Between extensions, the leftmost range wins for rules, and the extension
  whose id sorts first wins a fence language, a callout name or a link scheme.
- A callout the user or the theme sets for a name beats the extension's, which
  beats what is set for its kind.
- A fence renderer claims a block only while the cursor is outside it, and an
  unclosed fence is never claimed.

**Links.** `[text](destination)` shows only its text, so Zed's detection of
URLs under the pointer never saw it. `Addon::link_at` lets the addon say what
is under the pointer, which makes inline links clickable in live preview as a
side effect: web addresses open in the browser, relative paths open the file.

**Completions.** Zed MD wraps the editor's completion provider, forwards what
it does not handle, and hands the original back when live preview stops, unless
something replaced it in the meantime.

**Settings.** `visual_md.extensions.<id>` is free-form JSON. The host answers
`get-settings` with category `visual_md` from the id of the calling extension,
so an extension is never given another's entry.

**Checked in the running app.** The sample was installed into a throwaway
profile, as a symlink in its `extensions/installed` directory, and a document
using every rule and renderer was opened in the real binary on a headless
Wayland compositor, with screenshots. It showed the `sample-flow` SVG, the
`sample-table` Markdown table and the `sample-styled` text in place of their
blocks, `@ada` in blue, the `sample` callout with its purple accent and star
icon, and `opened` written to the extension's `events.log` with the right
counts. Removing the extension and starting again gave the same document as a
profile that never had it. The check found two bugs that no test had: an
extension that registers no language server lost its channel as soon as the
store finished loading it, and `opened` never reached an extension that loaded
after the document, which is every document restored at startup. Both are fixed
and tested.

Not checked in the running app, because they need keystrokes or the mouse and
there was no way to send either: running `uppercase` from the palette, as a
keymap binding and as `/uppercase`, the `[[` menu, hovering and clicking
`sample://docs`, the `saved` and `changed` events, and **zed: install dev
extension** (the symlink does what it does after compiling). The `:smile:`
replacements were present, but their emoji drew blank, as a literal emoji does
in that session, whose only emoji font is Noto's COLRv1 one. Ordinary code
fences in the same session lost their first line under the opening border
whether or not the extension was installed. The code that draws them is not
touched here, and whether it happens at the base was not checked.

**Limitations.**
- A call that times out is abandoned but not stopped. The extension keeps
  running it and answers nothing else until it finishes. The breaker is the
  backstop.
- Extensions can make network requests without declaring a capability, as in
  upstream Zed.
- Hidden and replacement text from rules is one line. Only fence renderers
  make blocks.
- A rule can name only the node kinds the Markdown grammar produces and Zed MD
  does not decorate itself: `html_tag`, the reference link kinds, `html_block`,
  `link_reference_definition`, `minus_metadata` and `plus_metadata`.
- A rendered fence's height is a first guess from the extension's hint, and
  the block is re-measured, so it can move when it scrolls into view.
- Settings have no schema, and the settings UI shows `visual_md.extensions` as
  a row that is edited in `settings.json`.
- PNG output is covered only by tests with fake hooks: the sample has no
  encoder.
- Only the Linux backend compiles here, so nothing was type-checked for macOS
  or Windows, and the extension paths were exercised on Linux only.

## Feature audit: Obsidian and Logseq parity

Written 2026-10-06, after M15, by reading `visual_md` against
[`visual-md-spec.md`](./visual-md-spec.md) (Obsidian Live Preview) and comparing
Logseq's editing model. It lists what the editor lacks for parity, **leaving out
database and knowledge-base (KB) features**, which are meant to come from
extensions. The line used: anything that needs a vault-wide index or a query
across documents is KB (backlinks, graph, queries and Dataview, block-reference
resolution, tag, alias and property pages, flashcards, link refactoring on
rename, journals and templates as file workflows). Finding a note by file name,
and reading one other note to show it, are editor features.

**Obsidian.** "Ext" is possible today with the M15 hooks, "Core" needs native
work, "API" needs new extension API.

| Gap | Where it stood at the audit | Fix |
| --- | --- | --- |
| Wikilinks: hidden brackets, alias, `#heading`, `#^id`, link style, unresolved style, ctrl-click | none | Core, **M16** |
| Tags as chips, click to search | none | Core, **M16** |
| `%%comments%%` | none | Core, **M16** |
| `^block-id` dimmed | none | Core, **M16** |
| Custom task marks `[/]` `[-]` | only `[ ]` and `[x]` | Core, **M16** |
| Highlight and inline code chips | no background | Core, **M16** |
| Embeds: `![[Note]]`, `#Heading`, sizes, audio, video, PDF | only a line that is one image | Core, M18 |
| Hover page preview, footnote hover | none | Core, M18 |
| Footnotes, math, YAML properties panel, embedded HTML | none | Core, M18 |
| Callouts: whole-row fold click, right-click type menu, per-type accent bar | chevron click only | Core, M18 |
| Tables: row lines, per-cell raw reveal | cells always raw | Core, M18 |
| Folding of headings and list items, saved per file | fold gutter off | Core, M17 |
| Built-in `[[`, `#`, `![[` autocomplete | extension-supplied names only | Core, M17 |
| Strike, highlight, code, link shortcuts | bold and italic only | Core, M17 |
| List editing: Tab to nest, Backspace on a marker, renumbering the source | Enter only | Core, M17 |
| Drag and drop to link or embed, image paste as `![[...]]` | drop opens a tab | Core, M17 |
| Reference-style links, Vim `ctrl-b` and `ctrl-i` | not handled, Vim wins | Core, M17 |
| Spellcheck, word count, export | none | Core or Ext, unscheduled |

**Logseq, beyond that.** Outliner editing (Tab and Shift-Tab move a bullet with
its children, Alt-Up and Alt-Down move a subtree, collapse, zoom into a block,
multi-block selection) is Core, M17, and needs folding first. `key:: value`
property lines, `TODO`/`DOING`/`DONE` markers, `[#A]` priorities, `SCHEDULED:` and
`^^highlight^^` are Ext through syntax rules, or Core with the properties panel
in M18. Auto-pairing `[[`, `((`, `**` and `==` is Core, M17. `((block refs))`,
`{{embed}}`, `{{query}}`, linked references and flashcards are KB. Inline macros
such as `{{video}}` need an inline widget API. Whiteboards and the graph view are
panes, out of scope.

**What KB extensions need from core.** An extension is given a note list only
inside a `[[` completion request, and cannot read other files, open or create a
file, hear about a rename, show a hover, draw inline or own a panel. Backlinks,
query results and a graph need all of that, each behind a declared capability:
listing and reading project files, opening and creating a file, file events,
hover content, inline widgets and some panel surface. That is M19.

**Roadmap.** M16 below. **M17:** editing tools and the outliner (folding with
saved state, subtree indent, outdent and move, Tab, Backspace and renumbering,
formatting shortcuts, auto-pair, paste and drop, built-in autocomplete from a
project note index, the Vim conflict, reference links). **M18:** block syntax
(frontmatter and `key::` properties panel, footnotes, math after a renderer
spike, an HTML subset, note and heading embeds with sizes, hover page preview,
callout and table polish). **M19:** the extension surface for KB features, with
a sample backlinks extension as the proof.

## M16: Block layout fixes, native links, tags and cursor-line syntax

**Today:** the audit above found two defects in how blocks are laid out, and
that wikilinks, tags, comments, block ids and custom task marks have no native
rendering, so a note-taking document is mostly plain text unless an extension
styles it.

**Deliverables**
1. Fence border and rule blocks replace only their own row, and blocks are
   placed where the text of their row is, under taller heading rows too.
2. Wikilinks `[[Note]]`, `[[Note|alias]]`, `[[Note#Heading]]` drawn as links,
   resolved against the project's notes by file name, opened with ctrl-click,
   and drawn muted with a wavy underline when the note does not exist.
3. Tags `#tag` and `#nested/tag` as chips that open a project search.
4. `%%comments%%` hidden away from the cursor and `^block-id` dimmed.
5. Task marks other than `[ ]` and `[x]` drawn as checkboxes with a symbol,
   configurable with `visual_md.task_marks`.
6. Highlights and inline code drawn as chips by default.
7. Settings for the new colors, in `settings.json` and the settings UI, and the
   documentation.

**Done when:** a document with every construct renders in the running app with
fences and rules correct under a heading, resolved and unresolved links look
different, and ctrl-click opens the right note or searches for the tag.

### As built

**Blocks.** A replace block covers every row up to the row its end is on, and
the ranges of fence lines and rules ended after their newline, so each
one-row block hid the next row too: the first line of the code under an opening
fence, and the line after a closing fence or a rule. The ranges now stop before
the newline, and a cursor at the start of the next line no longer counts as
being on the fence. Separately, `layout_blocks` placed a block at
`row * line_height` while text uses `row_y_offset`, which knows about taller
rows, so blocks drifted up under a heading; they use `row_y_offset` now. Both
were found by looking at the running app, and the tests that missed them only
counted blocks.

**Wikilinks.** They are scanned from text by `inline_scan`, shared with the
outline extensions are given. The planner treats the brackets like a link's:
hidden, or dimmed while a selection touches the link. A link to a note the
project has is drawn in the link color, and one to a note it lacks is muted with
a wavy underline. The project's notes are indexed by file name (`notes.rs`), once
per project, off the main thread, and editors are only told when a name came or
went. `[[Note]]` is `Note.md` wherever it is, in any case, with or without the
extension; `[[folder/Note]]` picks the note in that folder; with several matches
the folder of the current note wins, then the shortest path. Nothing is flagged
while the worktrees are still read, with no project, or for `[[#Heading]]`. An
extension that resolves wikilinks is asked first, and the project's notes are the
fallback.

**Tags.** A tag is `#` and a name that is not only digits, at the start of a
word, outside code and links. Ctrl-click runs a project search for it, through a
new `HoverLink::Action` in the editor.

**Comments and block ids.** A comment on one line is hidden until a selection
touches it, then dimmed. One over several lines of a paragraph is only dimmed,
since a fold cannot span lines. A comment is found within one paragraph, so one
cut by a blank line is not recognized. Nothing inside a comment is drawn as
anything else. A block id must end a line and follow whitespace.

**Task marks.** The grammar knows only `[ ]` and `[x]`, so the planner takes the
set of known marks as an input and claims `[c]` at the start of an item's text
only for those, followed by a space or the end of the line. The defaults are
`/ - > < ? ! * "` and `i`. A click on a marked checkbox checks the item.

**Chips.** Highlights use the theme's warning background and inline code its
element background by default. This reverses an earlier decision that they carry
no background, and the M14 rule that unset keys match today's rendering no longer
holds for these two keys; a transparent color turns a chip off, and the change is
its own commit so it can be dropped.

**Checked in the running app.** On a headless Wayland compositor with
screenshots: ordinary code fences show every code line with their borders in
place, the line after a fence and a rule is there, blocks line up under a second
heading; wikilinks show their text with the brackets hidden, an alias shows
without its target, `Missing note` is muted with a wavy underline, links to
existing notes and to a heading of the same note are in the link color, and tags
are chips, not the one in code and not `#12`. Not checked in the app, because
there was no way to send a click: ctrl-click on a wikilink and on a tag. Both are
covered by tests that ask the addon for the link and run its action.

**Limitations.**
- Clicking a link to a missing note does nothing; Obsidian creates the note.
- `[[Note#Heading]]` opens `Note` and does not scroll to the heading.
- `#[[multi word]]` is not a tag of its own; it shows as `#` followed by a link.
- Headings, block ids and front matter aliases are not looked at: extensions
  answer for them.
- A comment cut by a blank line is not recognized.
- The default chips change how every document already looks.

## M17: Editing tools and the outliner

**Today:** the audit above found that only bold and italic have shortcuts, that
`Enter` is the only list key, that Tab nests an ordered item by the wrong width,
that Vim's `ctrl-b` and `ctrl-i` win over the formatting shortcuts, and that
nothing folds. M17 closes these in three pull requests: editing tools, then
folding and the outliner, then completion, paste and drop.

### As built: editing tools

**Formatting shortcuts.** `ctrl-alt-x` (strikethrough), `ctrl-alt-u` (highlight),
`ctrl-alt-t` (inline code) and `ctrl-alt-n` (link), with `cmd` for `ctrl` on
macOS. They work like bold and italic: wrap a selection, unwrap one that is
already wrapped or sits inside the markers, and put a bare cursor between an
empty pair. Code uses a backtick run longer than any inside the selection, and a
padding space where the text starts or ends with a backtick. A link wraps the
selection as `[selection](url)` with `url` selected, and unwraps from inside an
existing link. Each is also an action for the command palette:
`visual_md::ToggleStrikethrough`, `ToggleHighlight`, `ToggleCode`, `ToggleLink`.

**List keys.** In live preview, on a plain cursor on a list item:
- `Tab` nests the item, its children included, under the item before it, by the
  width of that item's marker: 2 for `- `, 3 for `1. `. An item with no item
  before it, or a cursor in a quote, keeps the editor's own Tab.
- `Shift-Tab` takes the item and its children out a level. At the top level it
  keeps the editor's own behavior.
- `Backspace` at the start of an item's text removes a task checkbox first, else
  takes a nested item out a level, else removes the marker and leaves the text.
- `Alt-Up` and `Alt-Down` swap the item with the one before or after it, children
  included. Blank lines between items stay where they were.
- Ordered lists are renumbered in the source after `Enter`, `Tab`, `Shift-Tab`,
  `Backspace` and a move. A number with more or fewer digits moves the item's
  other lines by the difference so children stay under the text. Typing in the
  middle of a number is not renumbered; the display always is.
- With several cursors, a selection across lines, a read-only editor or a snippet
  tabstop to go to, the keys are left to the editor.

**Auto-pair.** `[[` already closed to `[[]]` because `[` pairs, which is now a
test. `=` and `_` surround a selection the way `*` and `~` do, so `==` over a
selection highlights it.

**Vim.** `ctrl-b` and `ctrl-i` in Vim's visual mode bold and italicize the
selection when live preview is on, outside macOS (where Vim's bindings use other
keys). In normal mode with a bare cursor Vim's page-up and jump-forward stay,
because a toggle there would insert `****`.

**Limitations.**
- A selection across lines is wrapped as one span, as bold has always been.
- List keys do not apply inside a quote; a list in a quote keeps the editor's keys.

### As built: folding and the outliner

**What folds.** A heading folds its section, up to the next heading of the same
or a higher level, and a list item folds what is nested under it: a list, a
second paragraph, a code block, a quote or a table. A wrapped line of the first
paragraph is not nesting. `fold_ranges.rs` finds these from the block parse,
once per edit. A range starts where the first line ends, trailing spaces
included, so that typing them does not move it, and stops after the last
character of the section, so the newline and the blank lines before the next
heading stay. Setext headings do not fold.

**Creases and state.** Every foldable range gets a crease, so the editor's
gutter arrow, `Fold`, `UnfoldLines`, `ToggleFold`, `FoldAll` and `UnfoldAll` find
it. A document with more than 2,000 foldable ranges only gets creases for what
is on screen and what is folded, so "fold all" folds only what has one. What is
folded is kept in the editor's fold map and lasts for the session: it is not
saved with the file and not restored when the file is opened again. Folds
follow edits; a section whose range changes is made again and folded again if
the section that started in the same place was folded. A folded section that
scrolls out of view stays folded. Moving a folded list item with Alt-Up or
Alt-Down unfolds it, because the move rewrites the lines the fold was anchored
in.

**Gutter.** The fold arrow appears on the cursor's row and while the gutter is
hovered, as for code, and follows the setting `gutter.folds`. The editor's guess
that any line followed by a more indented one can fold is off while live preview
is on; in prose it would put an arrow on every wrapped or nested line. The "⋯"
a folded section ends in unfolds it when clicked.

**Changes to the editor.** Four, all inert for any editor that does not opt in:
- A fold can be tagged `DecorativeFold` (a hidden marker, which is what every
  fold live preview makes from the plan is) or `TransientFold` (a folded
  section). Decorative folds do not count when asking whether a line is folded,
  are not removed by the unfold commands, and neither kind is saved with the
  file. Hidden markers used to be written to the fold table, which is probably
  why the application log showed unique constraint failures there; they no
  longer are.
- Looking up the crease on a row prefers one that can be folded over a decorative
  one, so a heading's section is found behind its hidden `#`.
- A block that replaces rows is dropped while those rows are folded away. It used
  to replace the row the fold is on, so folding a section with a code fence, a rule
  or a table in it blanked the heading.
- Unfolding no longer removes the blocks that replace rows in an editor that
  turns that off, which live preview does: its rules, fences, tables and images
  are such blocks, and `UnfoldAll` took them away until the text changed.

**Not in M17.** Zoom into a block and multi-block selection, saved fold state, and
`FoldAtLevel`, which only reaches ranges that have a crease.

**Not checked in the running app.** There was no way to send a click or a
key, so the arrow, the chip and the commands are covered by tests that call the
same functions the editor does.

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
| M15 | Extension hooks on par with Obsidian, Notion and Logseq | In progress: fence renderers and editor commands |

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

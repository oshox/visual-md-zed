use std::ops::Range;

use serde::{Deserialize, Serialize};

/// The syntax node kinds a syntax rule can match with `node`. Each is one that
/// Zed MD's Markdown grammar produces and that Zed MD does not itself decorate.
pub const VISUAL_MD_RULE_NODE_KINDS: &[&str] = &[
    "html_tag",
    "full_reference_link",
    "collapsed_reference_link",
    "shortcut_link",
    "html_block",
    "link_reference_definition",
    "minus_metadata",
    "plus_metadata",
];

/// Whether `color` is a hex color: `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa`.
pub fn is_hex_color(color: &str) -> bool {
    let Some(digits) = color.strip_prefix('#') else {
        return false;
    };
    matches!(digits.len(), 3 | 4 | 6 | 8) && digits.chars().all(|digit| digit.is_ascii_hexdigit())
}

/// How a run of text is styled by an extension.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VisualMdSpanStyle {
    /// The text color, as a hex string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// The background color, as a hex string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background_color: Option<String>,
    /// The name of a theme syntax token to take colors from. Explicit colors win.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme_token: Option<String>,
    /// The font weight, from 100 to 900.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_weight: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub italic: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub underline: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strikethrough: Option<bool>,
}

/// A style applied to a range of text, in UTF-8 byte offsets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualMdStyledSpan {
    pub range: Range<usize>,
    pub style: VisualMdSpanStyle,
}

/// Text with styles applied to ranges of it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VisualMdStyledText {
    pub text: String,
    pub spans: Vec<VisualMdStyledSpan>,
}

/// The format of an encoded image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisualMdImageFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
}

/// An encoded image.
#[derive(Clone, PartialEq, Eq)]
pub struct VisualMdImage {
    pub format: VisualMdImageFormat,
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for VisualMdImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VisualMdImage")
            .field("format", &self.format)
            .field("bytes", &format_args!("{} bytes", self.bytes.len()))
            .finish()
    }
}

/// What a fenced code block renderer produces, shown in place of the block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VisualMdFenceOutput {
    StyledText(VisualMdStyledText),
    Markdown(String),
    Svg(String),
    Image(VisualMdImage),
}

/// Whether the active theme is light or dark.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VisualMdAppearance {
    Light,
    Dark,
}

/// A fenced code block to render.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualMdFenceRequest {
    /// The language tag of the block, lowercased.
    pub language: String,
    /// The whole info string after the opening fence.
    pub info: String,
    /// The text between the fences.
    pub content: String,
    pub appearance: VisualMdAppearance,
    /// The path of the document, if it has one.
    pub path: Option<String>,
}

/// The result of rendering a fenced code block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualMdFenceResult {
    pub output: VisualMdFenceOutput,
    /// How many editor rows the output is expected to need, used until the
    /// block has been laid out.
    pub height_hint: Option<u32>,
}

/// The document an editor command runs on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualMdCommandContext {
    /// The whole text of the document.
    pub text: String,
    /// The selections, in UTF-8 byte offsets into `text`.
    pub selections: Vec<Range<usize>>,
    /// The path of the document, if it has one.
    pub path: Option<String>,
}

/// A replacement of a range of text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualMdTextEdit {
    /// The range to replace, in the coordinates of the text the command ran on.
    pub range: Range<usize>,
    pub new_text: String,
}

/// What an editor command changes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VisualMdCommandResult {
    /// The edits to apply, all in the coordinates of the original text.
    pub edits: Vec<VisualMdTextEdit>,
    /// The selections to leave afterwards, in the coordinates of the text after
    /// the edits.
    pub selections: Option<Vec<Range<usize>>>,
    /// A message for the user.
    pub message: Option<String>,
}

/// A piece of text that matched one of an extension's dynamic syntax rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualMdRuleMatch {
    /// The matched text.
    pub text: String,
    /// The range of each capture group within `text`, group 0 first. A group
    /// that took no part in the match is `None`.
    pub captures: Vec<Option<Range<usize>>>,
}

/// Text to show in place of a range of a match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualMdReplacement {
    /// The range to replace, in the coordinates of the matched text.
    pub range: Range<usize>,
    pub text: String,
}

/// What a dynamic rule makes of one match. Every range is in the coordinates
/// of that match's text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VisualMdRuleOutput {
    pub spans: Vec<VisualMdStyledSpan>,
    pub hidden: Vec<Range<usize>>,
    pub replacements: Vec<VisualMdReplacement>,
}

/// A heading of a document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualMdOutlineHeading {
    /// From 1 to 6.
    pub level: u8,
    pub text: String,
    pub range: Range<usize>,
}

/// How a link was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisualMdLinkStyle {
    /// `[text](destination)`.
    Inline,
    /// `[[name]]`.
    Wikilink,
    /// `![[name]]`.
    Embed,
}

/// A link or an embed in a document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualMdOutlineLink {
    pub style: VisualMdLinkStyle,
    pub target: String,
    pub text: Option<String>,
    pub range: Range<usize>,
}

/// A `#tag` in a document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualMdOutlineTag {
    pub name: String,
    pub range: Range<usize>,
}

/// A task list item of a document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualMdOutlineTask {
    pub text: String,
    pub checked: bool,
    pub range: Range<usize>,
}

/// The structure of a document. Every range is in UTF-8 bytes of it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VisualMdOutline {
    pub headings: Vec<VisualMdOutlineHeading>,
    pub links: Vec<VisualMdOutlineLink>,
    pub tags: Vec<VisualMdOutlineTag>,
    pub tasks: Vec<VisualMdOutlineTask>,
    /// The front matter between the `---` lines a document starts with.
    pub frontmatter: Option<String>,
}

/// What happened to a document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisualMdDocumentEventKind {
    Opened,
    Saved,
    Changed,
}

/// Something that happened to a document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualMdDocumentEvent {
    pub kind: VisualMdDocumentEventKind,
    pub path: Option<String>,
    pub outline: VisualMdOutline,
}

/// A link the user is pointing at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualMdLinkRequest {
    /// The scheme of an inline link's destination, lowercased.
    pub scheme: Option<String>,
    pub target: String,
    pub wikilink: bool,
    pub path: Option<String>,
}

/// Where a link leads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VisualMdLinkTarget {
    Url(String),
    /// An absolute path, or one relative to the document.
    File(String),
}

/// The user is completing the name of a wikilink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualMdCompletionRequest {
    pub query: String,
    pub path: Option<String>,
    /// The project's Markdown files, relative to its root.
    pub files: Vec<String>,
}

/// A suggested completion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualMdCompletionItem {
    pub label: String,
    pub detail: Option<String>,
    pub insert_text: String,
}

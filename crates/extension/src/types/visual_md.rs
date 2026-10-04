use std::ops::Range;

/// How a run of text is styled by an extension.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VisualMdSpanStyle {
    /// The text color, as a hex string.
    pub color: Option<String>,
    /// The background color, as a hex string.
    pub background_color: Option<String>,
    /// The name of a theme syntax token to take colors from. Explicit colors win.
    pub theme_token: Option<String>,
    /// The font weight, from 100 to 900.
    pub font_weight: Option<u16>,
    pub italic: Option<bool>,
    pub underline: Option<bool>,
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

//! The fonts, sizes and colors Markdown live preview paints with.
//!
//! [`ResolvedStyle::resolve`] turns the `visual_md` settings, the active theme
//! and the editor's font settings into concrete values. Every value follows the
//! same precedence: the `visual_md` setting, then the `visual_md.*` token in
//! the active theme's `syntax` map, then the default live preview has always
//! had. A setting key `x` is read from the theme as the `color` of the token
//! `visual_md.x`, and a key `x.background` as the `background_color` of that
//! same token. Fonts and sizes are not theme-able.

use std::collections::HashMap;
use std::str::FromStr as _;
use std::sync::Arc;

use gpui::{
    AbsoluteLength, App, FontFamilyName, FontStyle, FontWeight, HighlightStyle, Hsla, Pixels,
    SharedString, StrikethroughStyle, TextStyleRefinement, UnderlineStyle, px, relative, rgb,
};
use icons::IconName;
use settings::{
    IntoGpui as _, Settings as _, ThemeColor, VisualMdCalloutColorsContent, VisualMdCalloutContent,
    VisualMdColorsContent, VisualMdSettingsContent,
};
use theme::{ActiveTheme as _, SyntaxTheme};
use util::ResultExt as _;

use crate::extensions::{ExtensionCallout, ExtensionCalloutIcon, VisualMdExtensions};
use crate::plan::CalloutKind;

/// How much larger than the prose font each heading level is, H1 first.
const DEFAULT_HEADING_SCALES: [f32; 6] = [1.8, 1.5, 1.3, 1.15, 1.05, 1.0];

/// The color of the dimmed Markdown syntax shown on the cursor's line.
const DEFAULT_MARKER_COLOR: u32 = 0x6b7280;

/// A change this small in a size ratio is not worth restyling for.
const SCALE_EPSILON: f32 = 1e-3;

/// How strongly a tag's color tints its chip when no background is set.
const TAG_BACKGROUND_OPACITY: f32 = 0.15;

/// Where a callout's icon is drawn from.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CalloutIcon {
    /// A path in the app's bundled assets, such as `icons/info.svg`.
    Asset(SharedString),
    /// An `.svg` file on disk, as an extension ships its own.
    External(SharedString),
}

impl CalloutIcon {
    /// The bundled asset path, for tests that pin the built-in icons.
    #[cfg(test)]
    pub(crate) fn asset_path(&self) -> Option<&str> {
        match self {
            Self::Asset(path) => Some(path.as_ref()),
            Self::External(_) => None,
        }
    }
}

/// The icon, accent and background of one callout type, and the title an
/// extension gave it, if it did.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CalloutLook {
    pub icon: CalloutIcon,
    pub accent: Hsla,
    pub background: Hsla,
    /// Shown instead of the capitalized type name.
    pub title: Option<SharedString>,
}

/// A callout type an extension registered, with its colors and icon parsed.
#[derive(Clone, Debug, PartialEq)]
struct ExtensionCalloutStyle {
    title: Option<SharedString>,
    kind: Option<CalloutKind>,
    icon: Option<CalloutIcon>,
    accent: Option<Hsla>,
    background: Option<Hsla>,
}

impl ExtensionCalloutStyle {
    fn resolve(callout: &ExtensionCallout) -> Self {
        let color = |color: &Option<String>| {
            color
                .as_deref()
                .and_then(|color| theme::try_parse_color(color).log_err())
        };
        Self {
            title: callout.title.clone().map(SharedString::from),
            kind: callout.kind,
            icon: callout.icon.as_ref().and_then(|icon| match icon {
                ExtensionCalloutIcon::Name(name) => IconName::from_str(name)
                    .map_err(|_| anyhow::anyhow!("unknown callout icon {name:?}"))
                    .log_err()
                    .map(|icon| CalloutIcon::Asset(SharedString::from(icon.path()))),
                ExtensionCalloutIcon::Svg(path) => Some(CalloutIcon::External(SharedString::from(
                    path.to_string_lossy().into_owned(),
                ))),
            }),
            accent: color(&callout.accent),
            background: color(&callout.background),
        }
    }
}

/// Everything needed to look up the look of a callout by type name.
///
/// Custom type names are arbitrary, so the theme's tokens for them are read
/// when a look is requested rather than when the style is resolved.
#[derive(Clone, Debug)]
pub(crate) struct CalloutStyles {
    defaults: [CalloutLook; 5],
    callouts: HashMap<String, VisualMdCalloutContent>,
    colors: HashMap<String, VisualMdCalloutColorsContent>,
    /// The callout types extensions registered, by lowercased name.
    extension: HashMap<String, ExtensionCalloutStyle>,
    syntax: Arc<SyntaxTheme>,
}

impl PartialEq for CalloutStyles {
    fn eq(&self, other: &Self) -> bool {
        self.defaults == other.defaults
            && self.callouts == other.callouts
            && self.colors == other.colors
            && self.extension == other.extension
            && Arc::ptr_eq(&self.syntax, &other.syntax)
    }
}

impl CalloutStyles {
    fn resolve(
        settings: &VisualMdSettingsContent,
        colors: &VisualMdColorsContent,
        cx: &App,
    ) -> Self {
        let status = cx.theme().status();
        let defaults = [
            ("icons/info.svg", status.info, status.info_background),
            (
                "icons/sparkle.svg",
                status.success,
                status.success_background,
            ),
            (
                "icons/warning.svg",
                status.warning,
                status.warning_background,
            ),
            (
                "icons/x_circle_filled.svg",
                status.error,
                status.error_background,
            ),
            // An unrecognized `[!type]` still gets a real callout box, just a
            // neutral "additional information" treatment rather than a false
            // severity: `hint` is the status color already meant for that.
            ("icons/quote.svg", status.hint, status.hint_background),
        ]
        .map(|(icon_path, accent, background)| CalloutLook {
            icon: CalloutIcon::Asset(SharedString::from(icon_path)),
            accent,
            background,
            title: None,
        });

        Self {
            defaults,
            callouts: settings
                .callouts
                .iter()
                .flatten()
                .map(|(name, callout)| (name.to_lowercase(), callout.clone()))
                .collect(),
            colors: colors
                .callout
                .iter()
                .flatten()
                .map(|(name, colors)| (name.to_lowercase(), colors.clone()))
                .collect(),
            extension: cx
                .try_global::<VisualMdExtensions>()
                .map(|registry| {
                    registry
                        .callouts()
                        .iter()
                        .map(|(name, callout)| {
                            (name.clone(), ExtensionCalloutStyle::resolve(callout))
                        })
                        .collect()
                })
                .unwrap_or_default(),
            syntax: cx.theme().syntax().clone(),
        }
    }

    /// The look of a callout whose type was written as `raw_type_name`, which
    /// the planner resolved to `kind`.
    ///
    /// For each of accent, background and icon, the first of these that is
    /// set wins: the `callouts` entry for the written name, `colors.callout`
    /// for the written name, `colors.callout` for the kind's canonical name,
    /// then the theme's `visual_md.callout.<name>` token for the written name
    /// and for the canonical name, then the kind's default.
    ///
    /// A callout type an extension registered under the written name sits
    /// among them. Everything set for that name specifically, by the user in
    /// settings or by the theme, still wins over it, since the extension only
    /// supplies the type's own look. It wins over what is set for the kind,
    /// since a kind is only what the type starts from, and over the kind's
    /// default. The kind it declares replaces `Other` for a type of an unknown
    /// name, so the defaults and the kind-wide settings it starts from are its
    /// kind's.
    pub fn look(&self, kind: CalloutKind, raw_type_name: &str) -> CalloutLook {
        let name = raw_type_name.to_lowercase();
        let extension = self.extension.get(&name);
        let kind = match (kind, extension.and_then(|extension| extension.kind)) {
            (CalloutKind::Other, Some(declared)) => declared,
            (kind, _) => kind,
        };
        let canonical = kind.canonical_name();
        let default = &self.defaults[kind_index(kind)];

        let custom = self.callouts.get(&name);
        let named_colors = self.colors.get(&name);
        let canonical_colors = self.colors.get(canonical);
        let token = |token_name: &str| self.syntax.style_for_name(&token_for(token_name));
        // The theme's token for the written name outranks an extension's look,
        // so it is consulted ahead of it, when there is one. Without one it
        // keeps its place below the kind-wide settings, as it always had.
        let name_token = |field: fn(&HighlightStyle) -> Option<Hsla>| {
            extension.and(token(&name)).and_then(|style| field(&style))
        };

        let accent = custom
            .and_then(|custom| parse_color(custom.accent.as_ref()))
            .or_else(|| named_colors.and_then(|colors| parse_color(colors.accent.as_ref())))
            .or_else(|| name_token(|style| style.color))
            .or_else(|| extension.and_then(|extension| extension.accent))
            .or_else(|| canonical_colors.and_then(|colors| parse_color(colors.accent.as_ref())))
            .or_else(|| token(&name).and_then(|style| style.color))
            .or_else(|| token(canonical).and_then(|style| style.color))
            .unwrap_or(default.accent);
        let background = custom
            .and_then(|custom| parse_color(custom.background.as_ref()))
            .or_else(|| named_colors.and_then(|colors| parse_color(colors.background.as_ref())))
            .or_else(|| name_token(|style| style.background_color))
            .or_else(|| extension.and_then(|extension| extension.background))
            .or_else(|| canonical_colors.and_then(|colors| parse_color(colors.background.as_ref())))
            .or_else(|| token(&name).and_then(|style| style.background_color))
            .or_else(|| token(canonical).and_then(|style| style.background_color))
            .unwrap_or(default.background);
        let icon = custom
            .and_then(|custom| custom.icon.as_deref())
            .and_then(|icon| {
                IconName::from_str(icon)
                    .map_err(|_| anyhow::anyhow!("unknown callout icon {icon:?}"))
                    .log_err()
            })
            .map(|icon| CalloutIcon::Asset(SharedString::from(icon.path())))
            .or_else(|| extension.and_then(|extension| extension.icon.clone()))
            .unwrap_or_else(|| default.icon.clone());

        CalloutLook {
            icon,
            accent,
            background,
            title: extension.and_then(|extension| extension.title.clone()),
        }
    }
}

fn kind_index(kind: CalloutKind) -> usize {
    match kind {
        CalloutKind::Note => 0,
        CalloutKind::Tip => 1,
        CalloutKind::Warning => 2,
        CalloutKind::Danger => 3,
        CalloutKind::Other => 4,
    }
}

fn token_for(name: &str) -> String {
    format!("visual_md.callout.{name}")
}

fn parse_color(color: Option<&ThemeColor>) -> Option<Hsla> {
    color.and_then(|color| theme::try_parse_color(color).log_err())
}

/// Every font, size and color Markdown live preview paints with.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ResolvedStyle {
    pub prose_font_family: FontFamilyName,
    pub prose_font_weight: Option<FontWeight>,
    /// The size prose is shown at, only when `prose_font_size` is set.
    pub prose_font_size: Option<Pixels>,
    /// The line height multiplier, only when `prose_line_height` is set.
    pub prose_line_height: Option<f32>,
    pub code_font_family: FontFamilyName,
    /// The buffer font's weight, restored on code when prose has its own
    /// weight, which would otherwise carry over to it.
    pub code_font_weight: Option<FontWeight>,
    /// Code's size relative to prose, `None` when they are the same size.
    pub code_font_scale: Option<f32>,
    pub heading_font_family: Option<FontFamilyName>,
    pub heading_scales: [f32; 6],
    pub heading_weights: [FontWeight; 6],
    pub heading_colors: [Hsla; 6],
    pub bold_color: Hsla,
    pub italic_color: Hsla,
    pub strikethrough_color: Hsla,
    pub highlight_background: Option<Hsla>,
    pub inline_code_color: Option<Hsla>,
    pub inline_code_background: Option<Hsla>,
    pub link_color: Hsla,
    /// A wikilink to a note the project does not have.
    pub unresolved_link_color: Hsla,
    pub tag_color: Hsla,
    pub tag_background: Hsla,
    pub marker_color: Hsla,
    pub blockquote_bar_color: Hsla,
    pub rule_color: Hsla,
    pub table_border_color: Hsla,
    pub code_block_border_color: Hsla,
    pub code_block_background: Option<Hsla>,
    pub task_checked_color: Hsla,
    pub callouts: CalloutStyles,
}

impl ResolvedStyle {
    pub fn resolve(settings: &VisualMdSettingsContent, cx: &App) -> Self {
        let colors = settings.colors.clone().unwrap_or_default();
        let theme_colors = cx.theme().colors();
        let syntax = cx.theme().syntax();
        let theme_settings = theme_settings::ThemeSettings::get_global(cx);

        let token = |name: &str| syntax.style_for_name(&format!("visual_md.{name}"));
        let token_color = |name: &str| token(name).and_then(|style| style.color);
        let color_of = |setting: &Option<ThemeColor>, name: &str| {
            parse_color(setting.as_ref()).or_else(|| token_color(name))
        };
        let background_of = |setting: &Option<ThemeColor>, name: &str| {
            parse_color(setting.as_ref())
                .or_else(|| token(name).and_then(|style| style.background_color))
        };
        let foreground = theme_colors.editor_foreground;
        let border = theme_colors.border;

        let tag_color = token_color("tag").unwrap_or(theme_colors.text_accent);

        let heading_color = |level_setting: &Option<ThemeColor>, level: &str| {
            parse_color(level_setting.as_ref())
                .or_else(|| parse_color(colors.heading.as_ref()))
                .or_else(|| token_color(&format!("heading.{level}")))
                .or_else(|| token_color("heading"))
                .unwrap_or(foreground)
        };
        let heading_colors = [
            heading_color(&colors.heading_1, "1"),
            heading_color(&colors.heading_2, "2"),
            heading_color(&colors.heading_3, "3"),
            heading_color(&colors.heading_4, "4"),
            heading_color(&colors.heading_5, "5"),
            heading_color(&colors.heading_6, "6"),
        ];

        let heading_sizes = settings.heading_sizes.clone().unwrap_or_default();
        let configured_scales = [
            heading_sizes.h1,
            heading_sizes.h2,
            heading_sizes.h3,
            heading_sizes.h4,
            heading_sizes.h5,
            heading_sizes.h6,
        ];
        let heading_scales = std::array::from_fn(|index| {
            configured_scales
                .get(index)
                .copied()
                .flatten()
                .map(|scale| scale.0)
                .filter(|scale| scale.is_finite() && *scale > 0.0)
                .or_else(|| DEFAULT_HEADING_SCALES.get(index).copied())
                .unwrap_or(1.0)
        });
        let heading_weights = settings.heading_weights.clone().unwrap_or_default();
        let heading_weights = [
            heading_weights.h1,
            heading_weights.h2,
            heading_weights.h3,
            heading_weights.h4,
            heading_weights.h5,
            heading_weights.h6,
        ]
        .map(|weight| weight.map_or(FontWeight::BOLD, |weight| weight.into_gpui()));

        let valid_size =
            |size: settings::FontSize| Some(size.0).filter(|size| size.is_finite() && *size > 0.0);
        let adjusted_size = |size: Option<settings::FontSize>| {
            size.and_then(valid_size)
                .map(|size| theme_settings::adjusted_font_size(px(size), cx))
        };
        let prose_font_size = adjusted_size(settings.prose_font_size);
        let buffer_font_size = theme_settings.buffer_font_size(cx);
        // Code is sized against prose, which the editor's own font size is set
        // to when `prose_font_size` is. Unset, code keeps the buffer font size.
        let prose_reference = prose_font_size.unwrap_or(buffer_font_size);
        let code_font_scale = adjusted_size(settings.code_font_size)
            .or_else(|| prose_font_size.map(|_| buffer_font_size))
            .map(|size| f32::from(size) / f32::from(prose_reference))
            .filter(|scale| scale.is_finite() && (scale - 1.0).abs() > SCALE_EPSILON);

        let prose_font_weight = settings.prose_font_weight.map(|weight| weight.into_gpui());

        Self {
            prose_font_family: family_or(&settings.prose_font_family, &theme_settings.ui_font),
            prose_font_weight,
            prose_font_size,
            prose_line_height: settings
                .prose_line_height
                .map(|height| theme_settings::buffer_line_height_from_settings(height).value())
                .filter(|height| height.is_finite() && *height > 0.0),
            code_font_family: family_or(&settings.code_font_family, &theme_settings.buffer_font),
            code_font_weight: prose_font_weight.map(|_| theme_settings.buffer_font.weight),
            code_font_scale,
            heading_font_family: settings
                .heading_font_family
                .as_ref()
                .map(|family| FontFamilyName::new(family.as_ref())),
            heading_scales,
            heading_weights,
            heading_colors,
            bold_color: color_of(&colors.bold, "bold").unwrap_or(foreground),
            italic_color: color_of(&colors.italic, "italic").unwrap_or(foreground),
            strikethrough_color: color_of(&colors.strikethrough, "strikethrough")
                .unwrap_or(foreground),
            highlight_background: background_of(&colors.highlight_background, "highlight"),
            inline_code_color: color_of(&colors.inline_code, "inline_code"),
            inline_code_background: background_of(&colors.inline_code_background, "inline_code"),
            link_color: color_of(&colors.link, "link").unwrap_or(theme_colors.link_text_hover),
            unresolved_link_color: color_of(&None, "link.unresolved")
                .unwrap_or(theme_colors.text_muted),
            tag_color,
            tag_background: token("tag")
                .and_then(|style| style.background_color)
                .unwrap_or_else(|| tag_color.opacity(TAG_BACKGROUND_OPACITY)),
            marker_color: color_of(&colors.marker, "marker")
                .unwrap_or_else(|| rgb(DEFAULT_MARKER_COLOR).into()),
            blockquote_bar_color: color_of(&colors.blockquote_bar, "blockquote.bar")
                .unwrap_or(border),
            rule_color: color_of(&colors.rule, "rule").unwrap_or(border),
            table_border_color: color_of(&colors.table_border, "table.border").unwrap_or(border),
            code_block_border_color: color_of(&colors.code_block_border, "code_block.border")
                .unwrap_or(border),
            code_block_background: background_of(&colors.code_block_background, "code_block"),
            task_checked_color: color_of(&colors.task_checked, "task.checked")
                .unwrap_or(theme_colors.icon_accent),
            callouts: CalloutStyles::resolve(settings, &colors, cx),
        }
    }

    pub fn callout_look(&self, kind: CalloutKind, raw_type_name: &str) -> CalloutLook {
        self.callouts.look(kind, raw_type_name)
    }

    /// The refinement that gives the whole editor the prose size and line
    /// height, or `None` when neither is set.
    pub fn text_style_refinement(&self) -> Option<TextStyleRefinement> {
        if self.prose_font_size.is_none() && self.prose_line_height.is_none() {
            return None;
        }
        Some(TextStyleRefinement {
            font_size: self.prose_font_size.map(AbsoluteLength::Pixels),
            line_height: self.prose_line_height.map(relative),
            ..Default::default()
        })
    }

    pub fn dim_marker_style(&self) -> HighlightStyle {
        HighlightStyle {
            color: Some(self.marker_color),
            ..HighlightStyle::default()
        }
    }

    pub fn heading_style(&self, level: u8) -> HighlightStyle {
        let index = usize::from(level.clamp(1, 6) - 1);
        HighlightStyle {
            color: self.heading_colors.get(index).copied(),
            font_weight: self.heading_weights.get(index).copied(),
            font_size_scale: self.heading_scales.get(index).copied(),
            font_family: self.heading_font_family,
            ..HighlightStyle::default()
        }
    }

    pub fn bold_style(&self) -> HighlightStyle {
        HighlightStyle {
            color: Some(self.bold_color),
            font_weight: Some(FontWeight::BOLD),
            ..HighlightStyle::default()
        }
    }

    pub fn italic_style(&self) -> HighlightStyle {
        HighlightStyle {
            color: Some(self.italic_color),
            font_style: Some(FontStyle::Italic),
            ..HighlightStyle::default()
        }
    }

    pub fn strikethrough_style(&self) -> HighlightStyle {
        HighlightStyle {
            color: Some(self.strikethrough_color),
            strikethrough: Some(StrikethroughStyle {
                thickness: px(1.),
                color: None,
            }),
            ..HighlightStyle::default()
        }
    }

    /// Color only: no weight, slant or underline.
    pub fn link_style(&self) -> HighlightStyle {
        HighlightStyle {
            color: Some(self.link_color),
            ..HighlightStyle::default()
        }
    }

    /// A wikilink to a note that does not exist: muted, with a wavy underline.
    pub fn unresolved_link_style(&self) -> HighlightStyle {
        HighlightStyle {
            color: Some(self.unresolved_link_color),
            underline: Some(UnderlineStyle {
                thickness: px(1.),
                color: Some(self.unresolved_link_color),
                wavy: true,
            }),
            ..HighlightStyle::default()
        }
    }

    /// A `#tag`: a tinted chip in the tag's color.
    pub fn tag_style(&self) -> HighlightStyle {
        HighlightStyle {
            color: Some(self.tag_color),
            background_color: Some(self.tag_background),
            ..HighlightStyle::default()
        }
    }

    /// Inline code: the code font, its size relative to the surrounding prose,
    /// and a color or background only when one was resolved.
    pub fn inline_code_style(&self) -> HighlightStyle {
        HighlightStyle {
            font_family: Some(self.code_font_family),
            color: self.inline_code_color,
            background_color: self.inline_code_background,
            run_font_size_scale: self.code_font_scale,
            ..HighlightStyle::default()
        }
    }

    /// A fenced code block's content. Its size is a whole-row scale, so that
    /// blank lines in the block are the same height as the others.
    pub fn code_block_style(&self) -> HighlightStyle {
        HighlightStyle {
            font_family: Some(self.code_font_family),
            background_color: self.code_block_background,
            font_size_scale: self.code_font_scale,
            ..HighlightStyle::default()
        }
    }
}

fn family_or(setting: &Option<settings::FontFamilyName>, fallback: &gpui::Font) -> FontFamilyName {
    match setting {
        Some(family) => FontFamilyName::new(family.as_ref()),
        None => FontFamilyName::new(&fallback.family),
    }
}

#[cfg(test)]
mod tests {
    use editor::HighlightKey;
    use editor::test::editor_test_context::EditorTestContext;
    use gpui::TestAppContext;
    use settings::{VisualMdCalloutColorsContent, VisualMdCalloutContent};

    use crate::extensions::VisualMdExtensions;
    use crate::extensions::test_support::register;
    use crate::integration_tests::{init_test, markdown_language};

    use super::*;

    const TODO: &str = "[visual_md.callouts.todo]\ntitle = \"To do\"\nkind = \"tip\"\nicon = \"star\"\naccent = \"#a855f7\"\nbackground = \"#a855f71a\"\n";

    fn hex(color: &str) -> Hsla {
        theme::try_parse_color(color).expect("the test colors are valid")
    }

    fn look_with(
        cx: &mut TestAppContext,
        settings: &VisualMdSettingsContent,
        kind: CalloutKind,
        name: &str,
    ) -> CalloutLook {
        cx.update(|cx| ResolvedStyle::resolve(settings, cx).callout_look(kind, name))
    }

    fn look(cx: &mut TestAppContext, kind: CalloutKind, name: &str) -> CalloutLook {
        look_with(cx, &VisualMdSettingsContent::default(), kind, name)
    }

    fn status(cx: &mut TestAppContext) -> theme::StatusColors {
        cx.update(|cx| cx.theme().status().clone())
    }

    #[gpui::test]
    fn test_a_registered_callout_gives_its_title_icon_and_colors(cx: &mut TestAppContext) {
        init_test(cx);
        register(cx, "notes", TODO, None);

        let todo = look(cx, CalloutKind::Other, "todo");

        assert_eq!(todo.title.as_deref(), Some("To do"));
        assert_eq!(
            todo.icon,
            CalloutIcon::Asset(IconName::Star.path().to_string().into())
        );
        assert_eq!(todo.accent, hex("#a855f7"));
        assert_eq!(todo.background, hex("#a855f71a"));
        assert_eq!(
            look(cx, CalloutKind::Other, "ToDo"),
            todo,
            "names ignore case"
        );
    }

    #[gpui::test]
    fn test_other_callouts_are_unaffected(cx: &mut TestAppContext) {
        init_test(cx);
        let before = look(cx, CalloutKind::Other, "custom");
        let note_before = look(cx, CalloutKind::Note, "note");
        register(cx, "notes", TODO, None);

        assert_eq!(look(cx, CalloutKind::Other, "custom"), before);
        assert_eq!(look(cx, CalloutKind::Note, "note"), note_before);
        assert_eq!(look(cx, CalloutKind::Other, "custom").title, None);
    }

    #[gpui::test]
    fn test_the_declared_kind_gives_the_defaults_a_type_starts_from(cx: &mut TestAppContext) {
        init_test(cx);
        register(
            cx,
            "notes",
            "[visual_md.callouts.careful]\nkind = \"caution\"\n[visual_md.callouts.plain]\n",
            None,
        );
        let colors = status(cx);

        let careful = look(cx, CalloutKind::Other, "careful");
        assert_eq!(careful.accent, colors.warning);
        assert_eq!(careful.background, colors.warning_background);
        assert_eq!(careful.icon, CalloutIcon::Asset("icons/warning.svg".into()));

        let plain = look(cx, CalloutKind::Other, "plain");
        assert_eq!(plain.accent, colors.hint, "no kind means the generic look");
    }

    #[gpui::test]
    fn test_a_registered_callout_may_restyle_a_built_in_name_without_changing_its_kind(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        register(
            cx,
            "notes",
            "[visual_md.callouts.note]\naccent = \"#112233\"\nkind = \"danger\"\n",
            None,
        );
        let colors = status(cx);

        let note = look(cx, CalloutKind::Note, "note");

        assert_eq!(note.accent, hex("#112233"));
        assert_eq!(
            note.background, colors.info_background,
            "the planner's kind stands; the declared one only replaces `Other`"
        );
    }

    #[gpui::test]
    fn test_an_svg_icon_is_drawn_from_the_extensions_directory(cx: &mut TestAppContext) {
        init_test(cx);
        register(
            cx,
            "notes",
            "[visual_md.callouts.drawn]\nicon = \"icons/drawn.svg\"\n",
            None,
        );

        assert_eq!(
            look(cx, CalloutKind::Other, "drawn").icon,
            CalloutIcon::External("/extensions/installed/notes/icons/drawn.svg".into())
        );
    }

    #[gpui::test]
    fn test_an_unknown_icon_name_leaves_the_kinds_icon(cx: &mut TestAppContext) {
        init_test(cx);
        register(
            cx,
            "notes",
            "[visual_md.callouts.odd]\nkind = \"tip\"\nicon = \"definitely_not_an_icon\"\n",
            None,
        );

        assert_eq!(
            look(cx, CalloutKind::Other, "odd").icon,
            CalloutIcon::Asset("icons/sparkle.svg".into())
        );
    }

    #[gpui::test]
    fn test_a_callout_with_an_unknown_kind_is_not_registered(cx: &mut TestAppContext) {
        init_test(cx);
        register(
            cx,
            "notes",
            "[visual_md.callouts.bogus]\nkind = \"tipp\"\ntitle = \"Bogus\"\n[visual_md.callouts.fine]\nkind = \"info\"\n",
            None,
        );

        cx.update(|cx| {
            let callouts = cx.global::<VisualMdExtensions>().callouts();
            assert_eq!(callouts.keys().collect::<Vec<_>>(), vec!["fine"]);
            assert_eq!(callouts["fine"].kind, Some(CalloutKind::Note));
        });
    }

    #[gpui::test]
    fn test_the_first_extension_by_id_wins_a_contested_callout_name(cx: &mut TestAppContext) {
        init_test(cx);
        register(
            cx,
            "zeta",
            "[visual_md.callouts.todo]\ntitle = \"Zeta\"\n",
            None,
        );
        register(
            cx,
            "alpha",
            "[visual_md.callouts.todo]\ntitle = \"Alpha\"\n",
            None,
        );

        assert_eq!(
            look(cx, CalloutKind::Other, "todo").title.as_deref(),
            Some("Alpha")
        );
    }

    #[gpui::test]
    fn test_unregistering_removes_the_callout(cx: &mut TestAppContext) {
        init_test(cx);
        register(cx, "notes", TODO, None);
        cx.update(|cx| VisualMdExtensions::unregister("notes", cx));

        assert_eq!(look(cx, CalloutKind::Other, "todo").title, None);
    }

    fn settings_with_user_callout(accent: &str) -> VisualMdSettingsContent {
        VisualMdSettingsContent {
            callouts: Some(
                [(
                    "todo".to_string(),
                    VisualMdCalloutContent {
                        accent: Some(accent.into()),
                        ..Default::default()
                    },
                )]
                .into_iter()
                .collect(),
            ),
            ..Default::default()
        }
    }

    fn settings_with_colors(name: &str, accent: &str) -> VisualMdSettingsContent {
        VisualMdSettingsContent {
            colors: Some(VisualMdColorsContent {
                callout: Some(
                    [(
                        name.to_string(),
                        VisualMdCalloutColorsContent {
                            accent: Some(accent.into()),
                            background: None,
                        },
                    )]
                    .into_iter()
                    .collect(),
                ),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[gpui::test]
    fn test_what_the_user_sets_for_the_name_beats_the_extension(cx: &mut TestAppContext) {
        init_test(cx);
        register(cx, "notes", TODO, None);

        let by_callout = look_with(
            cx,
            &settings_with_user_callout("#00ff00"),
            CalloutKind::Other,
            "todo",
        );
        let by_colors = look_with(
            cx,
            &settings_with_colors("todo", "#0000ff"),
            CalloutKind::Other,
            "todo",
        );

        assert_eq!(by_callout.accent, hex("#00ff00"));
        assert_eq!(by_colors.accent, hex("#0000ff"));
        assert_eq!(
            by_callout.background,
            hex("#a855f71a"),
            "what the user left alone stays the extension's"
        );
    }

    #[gpui::test]
    fn test_the_extension_beats_what_is_set_for_the_whole_kind(cx: &mut TestAppContext) {
        init_test(cx);
        register(cx, "notes", TODO, None);
        register(
            cx,
            "other",
            "[visual_md.callouts.nocolor]\nkind = \"tip\"\n",
            None,
        );
        let kind_wide = settings_with_colors("tip", "#ff00ff");

        let todo = look_with(cx, &kind_wide, CalloutKind::Other, "todo");
        let nocolor = look_with(cx, &kind_wide, CalloutKind::Other, "nocolor");

        assert_eq!(
            todo.accent,
            hex("#a855f7"),
            "the extension set its own accent"
        );
        assert_eq!(
            nocolor.accent,
            hex("#ff00ff"),
            "without its own accent a type starts from what its kind is set to"
        );
    }

    #[gpui::test]
    fn test_the_themes_token_for_the_name_beats_the_extension(cx: &mut TestAppContext) {
        init_test(cx);
        register(cx, "notes", TODO, None);
        let themed = cx.update(|cx| {
            let mut resolved = ResolvedStyle::resolve(&Default::default(), cx);
            let token = HighlightStyle {
                color: Some(hex("#abcdef")),
                ..Default::default()
            };
            resolved.callouts.syntax = Arc::new(SyntaxTheme::new_test_styles([(
                "visual_md.callout.todo",
                token,
            )]));
            resolved.callout_look(CalloutKind::Other, "todo")
        });

        assert_eq!(themed.accent, hex("#abcdef"));
        assert_eq!(themed.background, hex("#a855f71a"));
    }

    #[gpui::test]
    async fn test_a_callout_in_the_editor_shows_the_extensions_title_and_background(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        register(cx, "notes", TODO, None);
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("ˇabove\n\n> [!todo] Buy milk\n> and eggs\n");
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| crate::refresh(editor, window, cx));

        let displayed = cx.display_text();
        assert!(displayed.contains("To do"), "{displayed:?}");
        assert!(!displayed.contains("Todo"), "{displayed:?}");
        let background = cx.update_editor(|editor, _, cx| {
            editor
                .text_highlights(HighlightKey::VisualMd(crate::KEY_CALLOUT_FIRST), cx)
                .and_then(|(style, _)| style.background_color)
        });
        assert_eq!(background, Some(hex("#a855f71a")));

        cx.update(|_, cx| VisualMdExtensions::unregister("notes", cx));
        cx.run_until_parked();
        let displayed = cx.display_text();
        assert!(displayed.contains("Todo"), "{displayed:?}");
        assert!(!displayed.contains("To do"), "{displayed:?}");
    }
}

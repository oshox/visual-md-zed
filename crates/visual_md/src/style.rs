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
    SharedString, StrikethroughStyle, TextStyleRefinement, px, relative, rgb,
};
use icons::IconName;
use settings::{
    IntoGpui as _, Settings as _, ThemeColor, VisualMdCalloutColorsContent, VisualMdCalloutContent,
    VisualMdColorsContent, VisualMdSettingsContent,
};
use theme::{ActiveTheme as _, SyntaxTheme};
use util::ResultExt as _;

use crate::plan::CalloutKind;

/// How much larger than the prose font each heading level is, H1 first.
const DEFAULT_HEADING_SCALES: [f32; 6] = [1.8, 1.5, 1.3, 1.15, 1.05, 1.0];

/// The color of the dimmed Markdown syntax shown on the cursor's line.
const DEFAULT_MARKER_COLOR: u32 = 0x6b7280;

/// A change this small in a size ratio is not worth restyling for.
const SCALE_EPSILON: f32 = 1e-3;

/// The icon, accent and background of one callout type.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CalloutLook {
    pub icon_path: SharedString,
    pub accent: Hsla,
    pub background: Hsla,
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
    syntax: Arc<SyntaxTheme>,
}

impl PartialEq for CalloutStyles {
    fn eq(&self, other: &Self) -> bool {
        self.defaults == other.defaults
            && self.callouts == other.callouts
            && self.colors == other.colors
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
            icon_path: SharedString::from(icon_path),
            accent,
            background,
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
    pub fn look(&self, kind: CalloutKind, raw_type_name: &str) -> CalloutLook {
        let name = raw_type_name.to_lowercase();
        let canonical = kind.canonical_name();
        let default = &self.defaults[kind_index(kind)];

        let custom = self.callouts.get(&name);
        let named_colors = self.colors.get(&name);
        let canonical_colors = self.colors.get(canonical);
        let token = |token_name: &str| self.syntax.style_for_name(&token_for(token_name));

        let accent = custom
            .and_then(|custom| parse_color(custom.accent.as_ref()))
            .or_else(|| named_colors.and_then(|colors| parse_color(colors.accent.as_ref())))
            .or_else(|| canonical_colors.and_then(|colors| parse_color(colors.accent.as_ref())))
            .or_else(|| token(&name).and_then(|style| style.color))
            .or_else(|| token(canonical).and_then(|style| style.color))
            .unwrap_or(default.accent);
        let background = custom
            .and_then(|custom| parse_color(custom.background.as_ref()))
            .or_else(|| named_colors.and_then(|colors| parse_color(colors.background.as_ref())))
            .or_else(|| canonical_colors.and_then(|colors| parse_color(colors.background.as_ref())))
            .or_else(|| token(&name).and_then(|style| style.background_color))
            .or_else(|| token(canonical).and_then(|style| style.background_color))
            .unwrap_or(default.background);
        let icon_path = custom
            .and_then(|custom| custom.icon.as_deref())
            .and_then(|icon| {
                IconName::from_str(icon)
                    .map_err(|_| anyhow::anyhow!("unknown callout icon {icon:?}"))
                    .log_err()
            })
            .map(|icon| SharedString::from(icon.path()))
            .unwrap_or_else(|| default.icon_path.clone());

        CalloutLook {
            icon_path,
            accent,
            background,
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

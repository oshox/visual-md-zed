//! A sample extension for Zed MD's live preview. It renders three kinds of
//! fenced code block and provides one editor command; see `README.md`.

use std::ops::Range;

use zed_extension_api::{self as zed, visual_md};

const MAX_CHAINS: usize = 50;
const MAX_STEPS_PER_CHAIN: usize = 40;

const MARGIN: i32 = 8;
const CHARACTER_WIDTH: i32 = 8;
const BOX_PADDING: i32 = 12;
const BOX_HEIGHT: i32 = 32;
const ARROW_WIDTH: i32 = 28;
const ROW_GAP: i32 = 16;

struct VisualMdSample;

impl zed::Extension for VisualMdSample {
    fn new() -> Self {
        Self
    }

    fn visual_md_render_fence(
        &self,
        renderer: String,
        request: visual_md::FenceRequest,
    ) -> Result<visual_md::FenceResult, String> {
        match renderer.as_str() {
            "sample-flow" => render_flow(&request.content, request.appearance),
            "sample-table" => render_table(&request.content),
            "sample-styled" => render_styled(&request.content),
            other => Err(format!("this extension does not render `{other}`")),
        }
    }

    fn visual_md_apply_rule(
        &self,
        rule: String,
        matches: Vec<visual_md::RuleMatch>,
    ) -> Result<Vec<visual_md::RuleOutput>, String> {
        match rule.as_str() {
            "emoji" => Ok(matches.iter().map(apply_emoji).collect()),
            other => Err(format!("this extension has no rule `{other}`")),
        }
    }

    fn visual_md_run_command(
        &self,
        command: String,
        context: visual_md::CommandContext,
    ) -> Result<visual_md::CommandResult, String> {
        match command.as_str() {
            "uppercase" => uppercase(context),
            other => Err(format!("this extension has no command `{other}`")),
        }
    }
}

zed::register_extension!(VisualMdSample);

/// The emoji `:name:` stands for, if the sample knows it.
fn emoji_for(name: &str) -> Option<&'static str> {
    Some(match name {
        "smile" => "🙂",
        "grin" => "😀",
        "heart" => "❤️",
        "tada" => "🎉",
        "rocket" => "🚀",
        "thumbsup" => "👍",
        "warning" => "⚠️",
        _ => return None,
    })
}

/// Hides the colons of a known `:name:` and shows its emoji in place of the
/// name. A name it does not know is left as written.
fn apply_emoji(rule_match: &visual_md::RuleMatch) -> visual_md::RuleOutput {
    let capture = |index: usize| rule_match.captures.get(index).copied().flatten();
    let (Some(opening), Some(name), Some(closing)) = (capture(1), capture(2), capture(3)) else {
        return empty_output();
    };
    let Some(emoji) = rule_match
        .text
        .get(name.start as usize..name.end as usize)
        .and_then(emoji_for)
    else {
        return empty_output();
    };
    visual_md::RuleOutput {
        spans: Vec::new(),
        hidden: vec![opening, closing],
        replacements: vec![visual_md::Replacement {
            range: name,
            text: emoji.to_string(),
        }],
    }
}

fn empty_output() -> visual_md::RuleOutput {
    visual_md::RuleOutput {
        spans: Vec::new(),
        hidden: Vec::new(),
        replacements: Vec::new(),
    }
}

fn escape_xml(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// Draws each line of `a -> b -> c` as a row of boxes joined by arrows.
fn render_flow(
    content: &str,
    appearance: visual_md::Appearance,
) -> Result<visual_md::FenceResult, String> {
    let chains: Vec<Vec<&str>> = content
        .lines()
        .map(|line| {
            line.split("->")
                .map(str::trim)
                .filter(|label| !label.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|chain| !chain.is_empty())
        .collect();
    if chains.is_empty() {
        return Err("write one or more chains such as `parse -> check -> emit`".to_string());
    }
    if chains.len() > MAX_CHAINS || chains.iter().any(|chain| chain.len() > MAX_STEPS_PER_CHAIN) {
        return Err(format!(
            "at most {MAX_CHAINS} chains of {MAX_STEPS_PER_CHAIN} steps are drawn"
        ));
    }

    let (stroke, fill, text) = match appearance {
        visual_md::Appearance::Light => ("#4a5568", "#edf2f7", "#1a202c"),
        visual_md::Appearance::Dark => ("#a0aec0", "#2d3748", "#edf2f7"),
    };

    let mut shapes = String::new();
    let mut width = 0;
    for (row, chain) in chains.iter().enumerate() {
        let top = MARGIN + row as i32 * (BOX_HEIGHT + ROW_GAP);
        let middle = top + BOX_HEIGHT / 2;
        let mut left = MARGIN;
        for (index, label) in chain.iter().enumerate() {
            if index > 0 {
                let from = left - ARROW_WIDTH + 4;
                let to = left - 4;
                shapes.push_str(&format!(
                    r#"<line x1="{from}" y1="{middle}" x2="{to}" y2="{middle}" stroke="{stroke}" stroke-width="2"/><polygon points="{to},{middle} {tip},{above} {tip},{below}" fill="{stroke}"/>"#,
                    tip = to - 7,
                    above = middle - 5,
                    below = middle + 5,
                ));
            }
            let box_width = label.chars().count() as i32 * CHARACTER_WIDTH + 2 * BOX_PADDING;
            shapes.push_str(&format!(
                r#"<rect x="{left}" y="{top}" width="{box_width}" height="{BOX_HEIGHT}" rx="6" fill="{fill}" stroke="{stroke}" stroke-width="1.5"/><text x="{center}" y="{middle}" fill="{text}" text-anchor="middle" dominant-baseline="central">{label}</text>"#,
                center = left + box_width / 2,
                label = escape_xml(label),
            ));
            left += box_width + ARROW_WIDTH;
        }
        width = width.max(left - ARROW_WIDTH + MARGIN);
    }
    let height =
        2 * MARGIN + chains.len() as i32 * BOX_HEIGHT + (chains.len() as i32 - 1) * ROW_GAP;

    Ok(visual_md::FenceResult {
        output: visual_md::FenceOutput::Svg(format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" font-family="sans-serif" font-size="14">{shapes}</svg>"#
        )),
        height_hint: Some(chains.len() as u32 * 3),
    })
}

/// Turns comma separated rows, the first of them the header, into a Markdown table.
fn render_table(content: &str) -> Result<visual_md::FenceResult, String> {
    let rows: Vec<Vec<&str>> = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.split(',').map(str::trim).collect())
        .collect();
    let Some(header) = rows.first() else {
        return Err("write comma separated rows, the first of them the header".to_string());
    };
    let columns = header.len();

    let row_markdown = |cells: &[&str]| {
        let mut line = String::from("|");
        for column in 0..columns {
            let cell = cells.get(column).copied().unwrap_or_default();
            line.push(' ');
            line.push_str(&cell.replace('|', "\\|"));
            line.push_str(" |");
        }
        line.push('\n');
        line
    };

    let mut markdown = row_markdown(header);
    markdown.push_str(&format!("|{}\n", " --- |".repeat(columns)));
    for row in rows.iter().skip(1) {
        markdown.push_str(&row_markdown(row));
    }

    Ok(visual_md::FenceResult {
        output: visual_md::FenceOutput::Markdown(markdown),
        height_hint: Some(rows.len() as u32 + 2),
    })
}

/// Shows the block's text with numbers, SHOUTING words and `#tags` picked out.
fn render_styled(content: &str) -> Result<visual_md::FenceResult, String> {
    let mut spans = Vec::new();
    let mut word_start: Option<usize> = None;
    for (index, character) in content
        .char_indices()
        .chain(std::iter::once((content.len(), ' ')))
    {
        if character.is_whitespace() {
            if let Some(start) = word_start.take()
                && let Some(word) = content.get(start..index)
                && let Some(style) = style_for_word(word)
            {
                spans.push(visual_md::StyledSpan {
                    range: zed::Range {
                        start: start as u32,
                        end: index as u32,
                    },
                    style,
                });
            }
        } else if word_start.is_none() {
            word_start = Some(index);
        }
    }

    Ok(visual_md::FenceResult {
        output: visual_md::FenceOutput::StyledText(visual_md::StyledText {
            text: content.to_string(),
            spans,
        }),
        height_hint: Some(content.lines().count().max(1) as u32),
    })
}

fn style_for_word(word: &str) -> Option<visual_md::SpanStyle> {
    let plain = visual_md::SpanStyle {
        color: None,
        background_color: None,
        theme_token: None,
        font_weight: None,
        italic: None,
        underline: None,
        strikethrough: None,
    };
    let is_number = word.chars().any(|character| character.is_ascii_digit())
        && word
            .chars()
            .all(|character| character.is_ascii_digit() || matches!(character, '.' | ',' | '-'));
    let is_shouting = word.chars().count() > 1
        && word.chars().any(char::is_alphabetic)
        && word
            .chars()
            .filter(|character| character.is_alphabetic())
            .all(char::is_uppercase);
    if is_number {
        Some(visual_md::SpanStyle {
            theme_token: Some("number".to_string()),
            ..plain
        })
    } else if is_shouting {
        Some(visual_md::SpanStyle {
            font_weight: Some(700),
            ..plain
        })
    } else if word.starts_with('#') && word.chars().count() > 1 {
        Some(visual_md::SpanStyle {
            theme_token: Some("keyword".to_string()),
            italic: Some(true),
            ..plain
        })
    } else {
        None
    }
}

/// The byte range of the line that contains `offset`, without its newline.
fn line_around(text: &str, offset: usize) -> Option<Range<usize>> {
    let before = text.get(..offset)?;
    let after = text.get(offset..)?;
    let start = before.rfind('\n').map_or(0, |newline| newline + 1);
    let end = after
        .find('\n')
        .map_or(text.len(), |newline| offset + newline);
    Some(start..end)
}

/// Uppercases each selection, or the line a cursor is on.
fn uppercase(context: visual_md::CommandContext) -> Result<visual_md::CommandResult, String> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for selection in &context.selections {
        let (start, end) = (selection.start as usize, selection.end as usize);
        let range = if start == end {
            line_around(&context.text, start)
        } else {
            Some(start..end)
        };
        let Some(range) = range else {
            return Err(format!(
                "the selection {start}..{end} is not in the document"
            ));
        };
        ranges.push(range);
    }
    ranges.sort_by_key(|range| range.start);
    // Two cursors on one line, or a cursor on a selected line, would give the
    // same text twice, which the editor refuses.
    let mut previous_end = 0;
    ranges.retain(|range| {
        let keep = range.start >= previous_end;
        if keep {
            previous_end = range.end;
        }
        keep
    });

    let mut edits = Vec::new();
    for range in ranges {
        let Some(original) = context.text.get(range.clone()) else {
            return Err(format!("the range {range:?} is not in the document"));
        };
        let uppercased = original.to_uppercase();
        if uppercased != original {
            edits.push(visual_md::TextEdit {
                range: zed::Range {
                    start: range.start as u32,
                    end: range.end as u32,
                },
                new_text: uppercased,
            });
        }
    }

    let message = match edits.len() {
        0 => "Nothing to uppercase".to_string(),
        1 => "Uppercased 1 place".to_string(),
        count => format!("Uppercased {count} places"),
    };
    Ok(visual_md::CommandResult {
        edits,
        selections: None,
        message: Some(message),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svg(result: Result<visual_md::FenceResult, String>) -> String {
        match result.map(|result| result.output) {
            Ok(visual_md::FenceOutput::Svg(svg)) => svg,
            other => panic!("expected an SVG, got {other:?}"),
        }
    }

    fn markdown(result: Result<visual_md::FenceResult, String>) -> String {
        match result.map(|result| result.output) {
            Ok(visual_md::FenceOutput::Markdown(markdown)) => markdown,
            other => panic!("expected Markdown, got {other:?}"),
        }
    }

    #[test]
    fn test_flow_draws_a_box_per_step_and_an_arrow_between_them() {
        let svg = svg(render_flow(
            "parse -> check -> emit",
            visual_md::Appearance::Dark,
        ));

        assert_eq!(svg.matches("<rect").count(), 3);
        assert_eq!(svg.matches("<polygon").count(), 2);
        assert!(svg.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""));
        assert!(svg.contains(">parse</text>") && svg.contains(">emit</text>"));
    }

    #[test]
    fn test_flow_stacks_chains_and_skips_blank_lines() {
        let svg = svg(render_flow(
            "a -> b\n\nc -> d -> e\n",
            visual_md::Appearance::Light,
        ));

        assert_eq!(svg.matches("<rect").count(), 5);
    }

    #[test]
    fn test_flow_escapes_labels() {
        let svg = svg(render_flow(
            "<script> -> a & b",
            visual_md::Appearance::Light,
        ));

        assert!(svg.contains("&lt;script&gt;"));
        assert!(svg.contains("a &amp; b"));
        assert!(!svg.contains("<script>"));
    }

    #[test]
    fn test_flow_uses_colors_that_suit_the_appearance() {
        let light = svg(render_flow("a -> b", visual_md::Appearance::Light));
        let dark = svg(render_flow("a -> b", visual_md::Appearance::Dark));

        assert_ne!(light, dark);
    }

    #[test]
    fn test_flow_explains_what_to_write_when_given_nothing() {
        assert!(render_flow("  \n", visual_md::Appearance::Dark).is_err());
        assert!(render_flow("->", visual_md::Appearance::Dark).is_err());
    }

    #[test]
    fn test_flow_refuses_more_than_it_can_reasonably_draw() {
        let too_many_chains = "a -> b\n".repeat(MAX_CHAINS + 1);
        let too_many_steps = vec!["a"; MAX_STEPS_PER_CHAIN + 1].join(" -> ");

        assert!(render_flow(&too_many_chains, visual_md::Appearance::Dark).is_err());
        assert!(render_flow(&too_many_steps, visual_md::Appearance::Dark).is_err());
    }

    #[test]
    fn test_table_becomes_markdown() {
        assert_eq!(
            markdown(render_table("Name, Role\nAda, Dev\nGrace")),
            "| Name | Role |\n| --- | --- |\n| Ada | Dev |\n| Grace |  |\n"
        );
    }

    #[test]
    fn test_table_escapes_pipes_and_trims_extra_cells() {
        assert_eq!(
            markdown(render_table("a|b, c\n1, 2, 3")),
            "| a\\|b | c |\n| --- | --- |\n| 1 | 2 |\n"
        );
    }

    #[test]
    fn test_table_needs_a_header() {
        assert!(render_table("\n \n").is_err());
    }

    #[test]
    fn test_styled_picks_out_numbers_shouting_and_tags() {
        let Ok(visual_md::FenceResult {
            output: visual_md::FenceOutput::StyledText(styled),
            ..
        }) = render_styled("build 42 FAILED on #ci é")
        else {
            panic!("expected styled text");
        };

        let styled_words: Vec<&str> = styled
            .spans
            .iter()
            .filter_map(|span| {
                styled
                    .text
                    .get(span.range.start as usize..span.range.end as usize)
            })
            .collect();
        assert_eq!(styled_words, vec!["42", "FAILED", "#ci"]);
        assert_eq!(styled.spans[0].style.theme_token.as_deref(), Some("number"));
        assert_eq!(styled.spans[1].style.font_weight, Some(700));
    }

    fn emoji_match(text: &str) -> visual_md::RuleMatch {
        let length = text.len() as u32;
        let range = |start: u32, end: u32| Some(zed::Range { start, end });
        visual_md::RuleMatch {
            text: text.to_string(),
            captures: vec![
                range(0, length),
                range(0, 1),
                range(1, length - 1),
                range(length - 1, length),
            ],
        }
    }

    #[test]
    fn test_a_known_emoji_hides_its_colons_and_replaces_its_name() {
        let output = apply_emoji(&emoji_match(":smile:"));

        assert_eq!(
            output
                .hidden
                .iter()
                .map(|range| (range.start, range.end))
                .collect::<Vec<_>>(),
            vec![(0, 1), (6, 7)]
        );
        assert_eq!(output.replacements.len(), 1);
        assert_eq!(
            (
                output.replacements[0].range.start,
                output.replacements[0].range.end
            ),
            (1, 6)
        );
        assert_eq!(output.replacements[0].text, "🙂");
    }

    #[test]
    fn test_an_unknown_emoji_is_left_as_written() {
        let output = apply_emoji(&emoji_match(":nonsense:"));

        assert!(output.hidden.is_empty() && output.replacements.is_empty());
    }

    #[test]
    fn test_a_match_without_its_groups_is_left_alone() {
        let output = apply_emoji(&visual_md::RuleMatch {
            text: ":smile:".to_string(),
            captures: vec![Some(zed::Range { start: 0, end: 7 })],
        });

        assert!(output.hidden.is_empty() && output.replacements.is_empty());
    }

    fn context(text: &str, selections: &[Range<u32>]) -> visual_md::CommandContext {
        visual_md::CommandContext {
            text: text.to_string(),
            selections: selections
                .iter()
                .map(|selection| zed::Range {
                    start: selection.start,
                    end: selection.end,
                })
                .collect(),
            path: None,
        }
    }

    fn edits(result: Result<visual_md::CommandResult, String>) -> Vec<(u32, u32, String)> {
        result
            .expect("the command succeeds")
            .edits
            .into_iter()
            .map(|edit| (edit.range.start, edit.range.end, edit.new_text))
            .collect()
    }

    #[test]
    fn test_uppercase_changes_the_selection() {
        assert_eq!(
            edits(uppercase(context("say hello now", &[4..9]))),
            vec![(4, 9, "HELLO".to_string())]
        );
    }

    #[test]
    fn test_uppercase_changes_the_line_when_nothing_is_selected() {
        assert_eq!(
            edits(uppercase(context("one\ntwo words\nthree", &[6..6]))),
            vec![(4, 13, "TWO WORDS".to_string())]
        );
    }

    #[test]
    fn test_uppercase_handles_several_cursors_without_overlapping_edits() {
        assert_eq!(
            edits(uppercase(context("ab\ncd\n", &[0..0, 1..1, 3..3]))),
            vec![(0, 2, "AB".to_string()), (3, 5, "CD".to_string())]
        );
    }

    #[test]
    fn test_uppercase_leaves_already_uppercase_text_alone() {
        let result = uppercase(context("ABC", &[0..3])).expect("the command succeeds");

        assert!(result.edits.is_empty());
        assert_eq!(result.message.as_deref(), Some("Nothing to uppercase"));
    }

    #[test]
    fn test_uppercase_rejects_a_selection_outside_the_document() {
        assert!(uppercase(context("abc", &[2..9])).is_err());
        assert!(uppercase(context("abc", &[9..9])).is_err());
    }

    #[test]
    fn test_uppercase_can_grow_the_text() {
        assert_eq!(
            edits(uppercase(context("straße", &[0..7]))),
            vec![(0, 7, "STRASSE".to_string())]
        );
    }
}

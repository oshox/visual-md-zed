//! Syntax rules that extensions declare: how a rule is compiled from its
//! manifest entry, how matches of it are found, and how an extension's answer
//! about a match is checked before anything is drawn from it.
//!
//! Like [`crate::plan`] this knows nothing about the editor. It works on text
//! and byte ranges, and the planner turns what it finds into decorations.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use extension::{
    VISUAL_MD_RULE_NODE_KINDS, VisualMdRuleOutput, VisualMdSpanStyle,
    VisualMdSyntaxRuleManifestEntry,
};
use regex::{Regex, RegexBuilder};

/// How much memory a rule's compiled pattern may take. Matching is linear in
/// the text whatever the pattern, so this is the bound on the rest of its cost.
const REGEX_SIZE_LIMIT: usize = 1024 * 1024;

/// The longest text a rule may put in place of a range, in characters.
pub const MAX_REPLACEMENT_CHARS: usize = 200;

#[derive(Debug)]
pub enum RuleMatcher {
    Pattern(Regex),
    /// A kind of syntax node.
    Node(&'static str),
}

/// A syntax rule ready to match. Which build of the extension it came from is
/// part of it, so what an extension answered for one build is never taken for
/// the answer of another.
#[derive(Debug)]
pub struct CompiledRule {
    pub extension_id: Arc<str>,
    pub generation: u64,
    pub id: Arc<str>,
    pub matcher: RuleMatcher,
    /// The capture groups to hide while the cursor is away from a match.
    pub hide: Vec<usize>,
    pub style: Option<VisualMdSpanStyle>,
    pub dynamic: bool,
}

impl CompiledRule {
    pub fn compile(
        extension_id: Arc<str>,
        generation: u64,
        entry: &VisualMdSyntaxRuleManifestEntry,
    ) -> Result<Self, String> {
        let matcher = match (&entry.pattern, &entry.node) {
            (Some(pattern), None) => {
                let regex = RegexBuilder::new(pattern)
                    .size_limit(REGEX_SIZE_LIMIT)
                    .build()
                    .map_err(|error| format!("the pattern does not compile: {error}"))?;
                if regex.is_match("") {
                    return Err("the pattern matches the empty string".to_string());
                }
                if let Some(group) = entry
                    .hide
                    .iter()
                    .find(|group| **group >= regex.captures_len())
                {
                    return Err(format!(
                        "it hides capture group {group}, but the pattern has {}",
                        regex.captures_len() - 1
                    ));
                }
                RuleMatcher::Pattern(regex)
            }
            (None, Some(node)) => {
                let kind = VISUAL_MD_RULE_NODE_KINDS
                    .iter()
                    .find(|kind| **kind == node.as_str())
                    .ok_or_else(|| format!("`{node}` is not a node kind a rule can match"))?;
                RuleMatcher::Node(kind)
            }
            _ => return Err("it needs exactly one of `pattern` and `node`".to_string()),
        };
        Ok(Self {
            extension_id,
            generation,
            id: entry.id.clone(),
            matcher,
            hide: entry.hide.clone(),
            style: entry.style.clone(),
            dynamic: entry.dynamic,
        })
    }
}

/// What the extension was asked, and so what its answer is filed under.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DynamicKey {
    pub extension_id: Arc<str>,
    pub generation: u64,
    pub rule: Arc<str>,
    pub text: String,
}

/// What an extension answered for a match, checked, and in the coordinates of
/// the match's own text so the answer serves the same text anywhere.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuleEffects {
    pub styled: Vec<(Range<usize>, VisualMdSpanStyle)>,
    pub hidden: Vec<Range<usize>>,
    pub replacements: Vec<(Range<usize>, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DynamicResult {
    Ready(Arc<RuleEffects>),
    /// The extension could not answer. It is not asked again for this text.
    Failed,
}

/// The rules in force and what the extensions have answered so far.
#[derive(Clone, Debug, Default)]
pub struct RuleSet {
    pub rules: Arc<[Arc<CompiledRule>]>,
    pub results: Arc<HashMap<DynamicKey, DynamicResult>>,
}

impl RuleSet {
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn node_rules<'a>(&'a self, kind: &'a str) -> impl Iterator<Item = &'a Arc<CompiledRule>> {
        self.rules
            .iter()
            .filter(move |rule| matches!(rule.matcher, RuleMatcher::Node(node) if node == kind))
    }

    pub fn has_pattern_rules(&self) -> bool {
        self.rules
            .iter()
            .any(|rule| matches!(rule.matcher, RuleMatcher::Pattern(_)))
    }
}

/// One place a rule matched.
#[derive(Debug)]
pub struct RuleHit {
    pub rule: Arc<CompiledRule>,
    /// Where the match is in the document.
    pub range: Range<usize>,
    pub text: String,
    /// Each capture group's range within `text`, group 0 first.
    pub captures: Vec<Option<Range<usize>>>,
}

/// Finds the matches of every pattern rule in `text`, a piece of the document
/// starting at `offset`. A match that is empty, contains a line break or touches
/// one of `excluded` (document ranges, such as inline code) is not a match.
pub fn find_pattern_hits(
    rules: &RuleSet,
    text: &str,
    offset: usize,
    excluded: &[Range<usize>],
) -> Vec<RuleHit> {
    let mut hits = Vec::new();
    for rule in rules.rules.iter() {
        let RuleMatcher::Pattern(regex) = &rule.matcher else {
            continue;
        };
        for captures in regex.captures_iter(text) {
            let Some(whole) = captures.get(0) else {
                continue;
            };
            let Some(matched) = text.get(whole.range()) else {
                continue;
            };
            let range = (whole.start() + offset)..(whole.end() + offset);
            if matched.is_empty()
                || matched.contains('\n')
                || excluded
                    .iter()
                    .any(|code| code.start < range.end && range.start < code.end)
            {
                continue;
            }
            hits.push(RuleHit {
                rule: rule.clone(),
                range,
                text: matched.to_string(),
                captures: captures
                    .iter()
                    .map(|group| {
                        group.map(|group| {
                            (group.start() - whole.start())..(group.end() - whole.start())
                        })
                    })
                    .collect(),
            });
        }
    }
    hits
}

/// A hit for a node a node rule matches. `range` is the node in the document.
pub fn node_hit(rule: &Arc<CompiledRule>, range: Range<usize>, text: &str) -> Option<RuleHit> {
    let node_text = text.get(range.clone())?;
    if node_text.is_empty() {
        return None;
    }
    Some(RuleHit {
        rule: rule.clone(),
        text: node_text.to_string(),
        captures: vec![Some(0..node_text.len())],
        range,
    })
}

/// Drops from `ranges` every one that is empty, out of bounds of `text`, off a
/// character boundary, holds a line break while `single_line` is set, or
/// overlaps an earlier one, and returns the rest in order.
fn valid_ranges(text: &str, mut ranges: Vec<Range<usize>>, single_line: bool) -> Vec<Range<usize>> {
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut covered_until = 0;
    ranges.retain(|range| {
        let Some(slice) = text.get(range.clone()) else {
            return false;
        };
        let is_valid = !slice.is_empty()
            && range.start >= covered_until
            && !(single_line && slice.contains('\n'));
        if is_valid {
            covered_until = range.end;
        }
        is_valid
    });
    ranges
}

/// Checks what an extension answered for `match_text`. Whatever is not usable is
/// dropped and the rest kept: ranges that are empty, out of bounds, off a
/// character boundary or overlapping an earlier one; hidden ranges and
/// replacements with a line break (a fold cannot span rows); replacements that
/// are too long or overlap a hidden range.
pub fn validate_output(match_text: &str, output: VisualMdRuleOutput) -> RuleEffects {
    let mut styled_spans = output.spans;
    styled_spans.sort_by_key(|span| (span.range.start, span.range.end));
    let mut covered_until = 0;
    styled_spans.retain(|span| {
        let is_valid = match_text
            .get(span.range.clone())
            .is_some_and(|slice| !slice.is_empty())
            && span.range.start >= covered_until;
        if is_valid {
            covered_until = span.range.end;
        }
        is_valid
    });

    let hidden = valid_ranges(match_text, output.hidden, true);

    let mut replacements = output.replacements;
    replacements.retain(|replacement| {
        !replacement.text.contains('\n')
            && replacement.text.chars().count() <= MAX_REPLACEMENT_CHARS
            && !hidden.iter().any(|hidden| {
                hidden.start < replacement.range.end && replacement.range.start < hidden.end
            })
    });
    replacements.sort_by_key(|replacement| (replacement.range.start, replacement.range.end));
    let replacement_ranges = valid_ranges(
        match_text,
        replacements
            .iter()
            .map(|replacement| replacement.range.clone())
            .collect(),
        true,
    );
    let replacements = replacement_ranges
        .into_iter()
        .filter_map(|range| {
            let replacement = replacements
                .iter()
                .find(|replacement| replacement.range == range)?;
            Some((range, replacement.text.clone()))
        })
        .collect();

    RuleEffects {
        styled: styled_spans
            .into_iter()
            .map(|span| (span.range, span.style))
            .collect(),
        hidden,
        replacements,
    }
}

#[cfg(test)]
mod tests {
    use extension::{VisualMdReplacement, VisualMdStyledSpan};

    use super::*;

    fn entry(pattern: Option<&str>, node: Option<&str>) -> VisualMdSyntaxRuleManifestEntry {
        VisualMdSyntaxRuleManifestEntry {
            id: "rule".into(),
            pattern: pattern.map(str::to_string),
            node: node.map(str::to_string),
            style: Some(VisualMdSpanStyle {
                italic: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn compile(entry: &VisualMdSyntaxRuleManifestEntry) -> Result<Arc<CompiledRule>, String> {
        CompiledRule::compile("notes".into(), 1, entry).map(Arc::new)
    }

    fn rule_set(entries: &[VisualMdSyntaxRuleManifestEntry]) -> RuleSet {
        RuleSet {
            rules: entries
                .iter()
                .map(|entry| compile(entry).expect("the rule compiles"))
                .collect(),
            results: Arc::default(),
        }
    }

    #[test]
    fn test_compiling_accepts_a_pattern_or_an_allowed_node() {
        assert!(compile(&entry(Some(r"@\w+"), None)).is_ok());
        assert!(compile(&entry(None, Some("html_tag"))).is_ok());
    }

    #[test]
    fn test_compiling_refuses_what_cannot_work() {
        let cases = [
            (entry(Some("("), None), "does not compile"),
            (entry(Some("a*"), None), "empty string"),
            (entry(Some(""), None), "empty string"),
            (entry(None, Some("heading")), "not a node kind"),
            (entry(None, None), "exactly one"),
            (entry(Some("x"), Some("html_tag")), "exactly one"),
            (
                VisualMdSyntaxRuleManifestEntry {
                    hide: vec![2],
                    ..entry(Some("(a)b"), None)
                },
                "hides capture group 2",
            ),
        ];
        for (entry, expected) in cases {
            let error = compile(&entry).expect_err("the rule should be refused");
            assert!(
                error.contains(expected),
                "{error:?} should contain {expected:?}"
            );
        }
    }

    #[test]
    fn test_compiling_refuses_a_pattern_that_is_too_big() {
        let huge = format!("({}){{1000}}", "[a-z]{100}");

        assert!(compile(&entry(Some(&huge), None)).is_err());
    }

    #[test]
    fn test_hide_may_name_group_zero_and_every_group() {
        let hiding = VisualMdSyntaxRuleManifestEntry {
            hide: vec![0, 1, 3],
            ..entry(Some("(:)(\\w+)(:)"), None)
        };

        assert!(compile(&hiding).is_ok());
    }

    #[test]
    fn test_hits_carry_document_ranges_and_relative_captures() {
        let rules = rule_set(&[entry(Some(r"(:)(\w+)(:)"), None)]);

        let hits = find_pattern_hits(&rules, "say :smile: now", 100, &[]);

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].range, 104..111);
        assert_eq!(hits[0].text, ":smile:");
        assert_eq!(
            hits[0].captures,
            vec![Some(0..7), Some(0..1), Some(1..6), Some(6..7)]
        );
    }

    #[test]
    fn test_a_group_that_did_not_take_part_is_none() {
        let rules = rule_set(&[entry(Some(r"a(b)?c"), None)]);

        let hits = find_pattern_hits(&rules, "ac abc", 0, &[]);

        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].captures, vec![Some(0..2), None]);
        assert_eq!(hits[1].captures, vec![Some(0..3), Some(1..2)]);
    }

    #[test]
    fn test_matches_with_a_line_break_or_in_excluded_text_are_not_hits() {
        let rules = rule_set(&[entry(Some(r"a\s+b"), None)]);

        assert!(find_pattern_hits(&rules, "a\nb", 0, &[]).is_empty());
        assert_eq!(find_pattern_hits(&rules, "a b", 0, &[]).len(), 1);
        assert!(find_pattern_hits(&rules, "a b", 0, &[1..2]).is_empty());
        assert!(find_pattern_hits(&rules, "a b", 0, &[2..9]).is_empty());
        assert_eq!(find_pattern_hits(&rules, "a b", 0, &[3..9]).len(), 1);
    }

    #[test]
    fn test_hits_come_in_rule_order() {
        let first = VisualMdSyntaxRuleManifestEntry {
            id: "first".into(),
            ..entry(Some("b"), None)
        };
        let second = VisualMdSyntaxRuleManifestEntry {
            id: "second".into(),
            ..entry(Some("a"), None)
        };
        let rules = rule_set(&[first, second]);

        let hits = find_pattern_hits(&rules, "ab", 0, &[]);

        assert_eq!(
            hits.iter()
                .map(|hit| hit.rule.id.as_ref())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
    }

    #[test]
    fn test_node_rules_are_found_by_kind() {
        let rules = rule_set(&[entry(None, Some("html_tag")), entry(Some("x"), None)]);

        assert_eq!(rules.node_rules("html_tag").count(), 1);
        assert_eq!(rules.node_rules("shortcut_link").count(), 0);
        assert!(rules.has_pattern_rules());
        assert!(!rule_set(&[entry(None, Some("html_tag"))]).has_pattern_rules());
    }

    #[test]
    fn test_a_node_hit_covers_the_whole_node() {
        let rules = rule_set(&[entry(None, Some("html_tag"))]);
        let rule = &rules.rules[0];

        let hit = node_hit(rule, 4..8, "see <b>x").expect("the node has text");

        assert_eq!(hit.text, "<b>x");
        assert_eq!(hit.captures, vec![Some(0..4)]);
        assert!(node_hit(rule, 4..4, "see <b>x").is_none());
        assert!(node_hit(rule, 4..99, "see <b>x").is_none());
    }

    fn span(range: Range<usize>) -> VisualMdStyledSpan {
        VisualMdStyledSpan {
            range,
            style: VisualMdSpanStyle::default(),
        }
    }

    fn replacement(range: Range<usize>, text: &str) -> VisualMdReplacement {
        VisualMdReplacement {
            range,
            text: text.to_string(),
        }
    }

    #[test]
    fn test_valid_output_is_kept_in_order() {
        let effects = validate_output(
            ":smile: ok",
            VisualMdRuleOutput {
                spans: vec![span(8..10), span(0..1)],
                hidden: vec![Range { start: 6, end: 7 }, Range { start: 0, end: 1 }],
                replacements: vec![replacement(1..6, "🙂")],
            },
        );

        assert_eq!(
            effects
                .styled
                .iter()
                .map(|(range, _)| range.clone())
                .collect::<Vec<_>>(),
            vec![0..1, 8..10]
        );
        assert_eq!(effects.hidden, vec![0..1, 6..7]);
        assert_eq!(effects.replacements, vec![(1..6, "🙂".to_string())]);
    }

    #[test]
    fn test_invalid_spans_are_dropped_and_the_rest_kept() {
        // `é` is two bytes, so 1..2 splits it.
        let reversed = Range { start: 5, end: 4 };
        let effects = validate_output(
            "aéb c",
            VisualMdRuleOutput {
                spans: vec![
                    span(0..1),
                    span(1..2),
                    span(2..2),
                    span(3..4),
                    span(3..5),
                    span(0..4),
                    span(9..12),
                    span(reversed),
                ],
                ..Default::default()
            },
        );

        assert_eq!(
            effects
                .styled
                .iter()
                .map(|(range, _)| range.clone())
                .collect::<Vec<_>>(),
            vec![0..1, 3..4]
        );
    }

    #[test]
    fn test_hidden_ranges_cannot_hold_a_line_break_or_overlap() {
        let effects = validate_output(
            "ab\ncd ef",
            VisualMdRuleOutput {
                hidden: vec![0..4, 3..5, 3..4, 6..8, 7..8],
                ..Default::default()
            },
        );

        assert_eq!(effects.hidden, vec![3..4, 6..8]);
    }

    #[test]
    fn test_replacements_are_single_line_short_and_clear_of_hidden_ranges() {
        let long = "x".repeat(MAX_REPLACEMENT_CHARS + 1);
        let exact = "y".repeat(MAX_REPLACEMENT_CHARS);
        let effects = validate_output(
            "abcdefghij\nk",
            VisualMdRuleOutput {
                hidden: vec![0..2],
                replacements: vec![
                    replacement(1..3, "overlaps hidden"),
                    replacement(2..4, "ok"),
                    replacement(4..5, "two\nlines"),
                    replacement(5..6, &long),
                    replacement(6..7, &exact),
                    replacement(9..12, "spans a line break"),
                    replacement(3..5, "overlaps the first kept one"),
                ],
                ..Default::default()
            },
        );

        assert_eq!(
            effects.replacements,
            vec![(2..4, "ok".to_string()), (6..7, exact)]
        );
    }

    #[test]
    fn test_nothing_valid_gives_no_effects() {
        let effects = validate_output("abc", VisualMdRuleOutput::default());

        assert_eq!(effects, RuleEffects::default());
    }
}

#[cfg(test)]
mod integration_tests {
    use editor::test::editor_test_context::EditorTestContext;
    use editor::{HighlightKey, ToOffset as _};
    use gpui::{HighlightStyle, TestAppContext};

    use crate::VisualMdAddon;
    use crate::extensions::VisualMdExtensions;
    use crate::extensions::test_support::register;
    use crate::integration_tests::{init_test, markdown_language};

    use super::*;

    const MENTION_RULE: &str = "[[visual_md.syntax_rules]]\nid = \"mention\"\npattern = '@\\w+'\nstyle = { color = \"#3b82f6\", font_weight = 600 }\n";
    const EMOJI_HIDE_RULE: &str =
        "[[visual_md.syntax_rules]]\nid = \"colons\"\npattern = '(:)(\\w+)(:)'\nhide = [1, 3]\n";

    async fn editor_showing(cx: &mut TestAppContext, text: &str) -> EditorTestContext {
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state(text);
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| crate::refresh(editor, window, cx));
        cx
    }

    fn highlighted(
        cx: &mut EditorTestContext,
        index: usize,
    ) -> Option<(HighlightStyle, Vec<Range<usize>>)> {
        cx.update_editor(|editor, _, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            editor
                .text_highlights(HighlightKey::VisualMdExtension(index), cx)
                // A cleared key keeps its entry, with no ranges.
                .filter(|(_, ranges)| !ranges.is_empty())
                .map(|(style, ranges)| {
                    (
                        style,
                        ranges
                            .iter()
                            .map(|range| {
                                range.start.to_offset(&snapshot).0..range.end.to_offset(&snapshot).0
                            })
                            .collect(),
                    )
                })
        })
    }

    fn folded(cx: &mut EditorTestContext) -> Vec<(Range<usize>, String)> {
        cx.update_editor(|editor, _, _| {
            editor
                .addon::<VisualMdAddon>()
                .map(|addon| {
                    addon
                        .folded_markers
                        .iter()
                        .map(|(range, key, _)| (range.clone(), key.clone()))
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    #[gpui::test]
    async fn test_a_registered_rule_styles_matches_in_the_editor(cx: &mut TestAppContext) {
        init_test(cx);
        register(cx, "notes", MENTION_RULE, None);
        let mut cx = editor_showing(cx, "ˇhi @ada and @bob\n").await;

        let (style, ranges) = highlighted(&mut cx, 0).expect("the matches are styled");

        assert_eq!(ranges, vec![3..7, 12..16]);
        assert_eq!(style.color, theme::try_parse_color("#3b82f6").ok());
        assert_eq!(style.font_weight, Some(gpui::FontWeight(600.)));
        assert!(highlighted(&mut cx, 1).is_none());
    }

    #[gpui::test]
    async fn test_hidden_groups_fold_away_until_the_cursor_touches_the_match(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        register(cx, "notes", EMOJI_HIDE_RULE, None);
        let mut cx = editor_showing(cx, "ˇsay :smile: now\n").await;

        let hidden = folded(&mut cx);
        assert_eq!(
            hidden
                .iter()
                .map(|(range, _)| range.clone())
                .collect::<Vec<_>>(),
            vec![4..5, 10..11]
        );

        cx.set_state("say :smˇile: now\n");
        cx.update_editor(|editor, window, cx| crate::refresh(editor, window, cx));
        assert!(
            folded(&mut cx).is_empty(),
            "touching the match shows its source"
        );
    }

    #[gpui::test]
    async fn test_each_distinct_style_gets_a_key_of_its_own(cx: &mut TestAppContext) {
        init_test(cx);
        register(
            cx,
            "notes",
            &format!(
                "{MENTION_RULE}[[visual_md.syntax_rules]]\nid = \"tag\"\npattern = '#\\w+'\nstyle = {{ italic = true }}\n"
            ),
            None,
        );
        let mut cx = editor_showing(cx, "ˇ@a #b @c\n").await;

        let (_, mentions) = highlighted(&mut cx, 0).expect("mentions are styled");
        let (tag_style, tags) = highlighted(&mut cx, 1).expect("tags are styled");

        assert_eq!(mentions, vec![0..2, 6..8]);
        assert_eq!(tags, vec![3..5]);
        assert_eq!(tag_style.font_style, Some(gpui::FontStyle::Italic));
        assert!(highlighted(&mut cx, 2).is_none());
    }

    #[gpui::test]
    async fn test_unregistering_the_extension_removes_its_styles_and_folds(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        register(
            cx,
            "notes",
            &format!("{MENTION_RULE}{EMOJI_HIDE_RULE}"),
            None,
        );
        let mut cx = editor_showing(cx, "ˇ@ada :smile:\n").await;
        assert!(highlighted(&mut cx, 0).is_some());
        assert!(!folded(&mut cx).is_empty());

        cx.update(|_, cx| VisualMdExtensions::unregister("notes", cx));
        cx.run_until_parked();

        assert!(highlighted(&mut cx, 0).is_none());
        assert!(folded(&mut cx).is_empty());
    }

    #[gpui::test]
    async fn test_registering_the_extension_again_replaces_its_rules(cx: &mut TestAppContext) {
        init_test(cx);
        register(cx, "notes", MENTION_RULE, None);
        let mut cx = editor_showing(cx, "ˇ@ada #tag\n").await;
        assert_eq!(
            highlighted(&mut cx, 0).map(|(_, ranges)| ranges),
            Some(vec![0..4])
        );

        register(
            &mut cx,
            "notes",
            "[[visual_md.syntax_rules]]\nid = \"tag\"\npattern = '#\\w+'\nstyle = { italic = true }\n",
            None,
        );
        cx.run_until_parked();

        assert_eq!(
            highlighted(&mut cx, 0).map(|(_, ranges)| ranges),
            Some(vec![5..9])
        );
        assert!(highlighted(&mut cx, 1).is_none());
    }

    #[gpui::test]
    async fn test_a_rule_that_cannot_compile_is_skipped_and_the_others_work(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        register(
            cx,
            "notes",
            &format!(
                "[[visual_md.syntax_rules]]\nid = \"broken\"\npattern = '('\nstyle = {{ italic = true }}\n{MENTION_RULE}"
            ),
            None,
        );

        cx.update(|cx| {
            let rules = cx.global::<VisualMdExtensions>().syntax_rules();
            assert_eq!(rules.len(), 1);
            assert_eq!(rules[0].id.as_ref(), "mention");
        });
    }

    #[gpui::test]
    async fn test_rules_are_ordered_by_extension_then_declaration(cx: &mut TestAppContext) {
        init_test(cx);
        let two_rules = format!(
            "{MENTION_RULE}[[visual_md.syntax_rules]]\nid = \"tag\"\npattern = '#\\w+'\nstyle = {{ italic = true }}\n"
        );
        register(cx, "zeta", &two_rules, None);
        register(cx, "alpha", &two_rules, None);

        cx.update(|cx| {
            let rules = cx.global::<VisualMdExtensions>().syntax_rules();
            assert_eq!(
                rules
                    .iter()
                    .map(|rule| format!("{}.{}", rule.extension_id, rule.id))
                    .collect::<Vec<_>>(),
                vec!["alpha.mention", "alpha.tag", "zeta.mention", "zeta.tag"]
            );
        });
    }

    /// Hiding things in a document that has every kind of fold of Zed MD's own
    /// must neither panic the editor's fold machinery nor change Zed MD's folds.
    #[gpui::test]
    async fn test_rules_that_hide_text_coexist_with_every_built_in_fold(cx: &mut TestAppContext) {
        init_test(cx);
        register(
            cx,
            "notes",
            "[[visual_md.syntax_rules]]\nid = \"all\"\npattern = '[^a-z ]+'\nhide = [0]\n",
            None,
        );
        let document = "ˇ# Heading\n\n- [ ] task **bold** and ==marked==\n1. item `code`\n\n> [!note] title\n> body > text\n\n| a | b |\n|---|---|\n| c | d |\n";
        let mut cx = editor_showing(cx, document).await;

        let with_rule = folded(&mut cx);
        cx.update(|_, cx| VisualMdExtensions::unregister("notes", cx));
        cx.run_until_parked();
        let without_rule = folded(&mut cx);

        for (range, key) in &without_rule {
            assert!(
                with_rule.contains(&(range.clone(), key.clone())),
                "Zed MD's own fold {range:?} ({key}) must survive the rule"
            );
        }
    }
}

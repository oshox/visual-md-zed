//! Inline syntax from note-taking apps that the Markdown grammar knows nothing
//! about: `[[wikilinks]]`, `#tags`, `%%comments%%` and `^block-ids`. They are
//! found by scanning text, and the
//! scanners are shared by the planner, which decorates what they find, and the
//! outline, which tells extensions about it.
//!
//! Like everything the planner reads, the text scanned is one inline node's text
//! with its code spans, links and quote markers still in place, so each scanner
//! is given the ranges to leave alone. Every range returned is shifted by the
//! `offset` given, to be in the coordinates of the document.

use std::ops::Range;
use std::sync::LazyLock;

use regex::Regex;

/// A `[[target]]`, `[[target|alias]]` or `![[target]]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wikilink {
    /// All of it, from the `!` of an embed to the closing `]]`.
    pub range: Range<usize>,
    pub is_embed: bool,
    /// What the link shows: the alias if there is one, otherwise the target as
    /// written, without the padding around either.
    pub visible: Range<usize>,
    /// Everything before what is shown: the brackets, a `!`, and a target and its
    /// `|` when there is an alias.
    pub prefix: Range<usize>,
    /// Everything after what is shown: the closing brackets, and padding.
    pub suffix: Range<usize>,
    /// The note, heading or block it points at, as written, such as `Note`,
    /// `Note#Heading` or `#Heading`.
    pub target: String,
    pub alias: Option<String>,
}

/// Finds every wikilink in `inline_text` that does not start inside one of
/// `excluded`, which are in document coordinates.
pub fn find_wikilinks(
    inline_text: &str,
    offset: usize,
    excluded: &[Range<usize>],
) -> Vec<Wikilink> {
    let mut found = Vec::new();
    let mut search_from = 0;
    while let Some(open) = inline_text
        .get(search_from..)
        .and_then(|rest| rest.find("[["))
        .map(|relative| search_from + relative)
    {
        search_from = open + 2;
        if excluded
            .iter()
            .any(|range| range.contains(&(open + offset)))
        {
            continue;
        }
        let Some(inner_end) = inline_text
            .get(open + 2..)
            .and_then(|rest| rest.find("]]"))
            .map(|close| open + 2 + close)
        else {
            break;
        };
        let Some(inner) = inline_text.get(open + 2..inner_end) else {
            continue;
        };
        if inner.is_empty() || inner.contains(['\n', '[']) {
            continue;
        }

        let is_embed = inline_text
            .get(..open)
            .is_some_and(|before| before.ends_with('!'));
        let start = if is_embed { open - 1 } else { open };
        let end = inner_end + 2;
        search_from = end;

        let inner_start = open + 2;
        let (target_text, alias_text) = match inner.split_once('|') {
            Some((target, alias)) => (target, Some(alias)),
            None => (inner, None),
        };
        let target = target_text.trim();
        if target.is_empty() {
            continue;
        }
        let trimmed_range = |text: &str, text_start: usize| {
            let leading = text.len() - text.trim_start().len();
            let from = text_start + leading;
            from..from + text.trim().len()
        };
        let alias_range = alias_text
            .map(|alias| trimmed_range(alias, inner_start + target_text.len() + 1))
            .filter(|range| !range.is_empty());
        let visible = alias_range
            .clone()
            .unwrap_or_else(|| trimmed_range(target_text, inner_start));
        let alias = alias_range
            .and_then(|range| inline_text.get(range))
            .map(str::to_string);

        found.push(Wikilink {
            range: start + offset..end + offset,
            is_embed,
            prefix: start + offset..visible.start + offset,
            suffix: visible.end + offset..end + offset,
            visible: visible.start + offset..visible.end + offset,
            target: target.to_string(),
            alias,
        });
    }
    found
}

/// A `%%comment%%`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    /// All of it, from the opening `%%` to the closing one.
    pub range: Range<usize>,
    /// Whether it runs over more than one line of the text it is in.
    pub is_multiline: bool,
}

/// Finds every `%%comment%%` in `inline_text` that does not start inside one of
/// `excluded`, which are in document coordinates. An empty `%%%%` is not one,
/// and a `%%` that is never closed is left alone.
pub fn find_comments(inline_text: &str, offset: usize, excluded: &[Range<usize>]) -> Vec<Comment> {
    let mut found = Vec::new();
    let mut search_from = 0;
    while let Some(open) = inline_text
        .get(search_from..)
        .and_then(|rest| rest.find("%%"))
        .map(|relative| search_from + relative)
    {
        search_from = open + 2;
        if excluded
            .iter()
            .any(|range| range.contains(&(open + offset)))
        {
            continue;
        }
        let Some(close) = inline_text
            .get(open + 2..)
            .and_then(|rest| rest.find("%%"))
            .map(|relative| open + 2 + relative)
        else {
            break;
        };
        if close == open + 2 {
            continue;
        }
        let end = close + 2;
        search_from = end;
        found.push(Comment {
            range: open + offset..end + offset,
            is_multiline: inline_text
                .get(open..end)
                .is_some_and(|comment| comment.contains('\n')),
        });
    }
    found
}

/// A block id, the `^id` that ends a line to name that block.
static BLOCK_ID_PATTERN: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(?:^|\s)(\^[A-Za-z0-9-]+)$").ok());

/// Finds the `^block-id` ending each line of `inline_text` that does not overlap
/// one of `excluded`, which are in document coordinates. It has to follow
/// whitespace or start the line, so `x^2` is not one.
pub fn find_block_ids(
    inline_text: &str,
    offset: usize,
    excluded: &[Range<usize>],
) -> Vec<Range<usize>> {
    let Some(pattern) = BLOCK_ID_PATTERN.as_ref() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut line_start = 0;
    for line in inline_text.split('\n') {
        let trimmed = line.trim_end();
        if let Some(id) = pattern
            .captures(trimmed)
            .and_then(|captures| captures.get(1))
        {
            let range = line_start + id.start() + offset..line_start + id.end() + offset;
            let is_excluded = excluded
                .iter()
                .any(|excluded| excluded.start < range.end && range.start < excluded.end);
            if !is_excluded {
                found.push(range);
            }
        }
        line_start += line.len() + 1;
    }
    found
}

/// A `#tag`, `#nested/tag`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    /// The name without the `#`.
    pub name: String,
    /// The `#` and the name.
    pub range: Range<usize>,
}

/// A `#` at the start of a word, then the name.
static TAG_PATTERN: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(?:^|[\s(\[{,;])#([\p{L}\p{N}_][\p{L}\p{N}_/-]*)").ok());

/// Finds every tag in `inline_text` that does not overlap one of `excluded`,
/// which are in document coordinates.
pub fn find_tags(inline_text: &str, offset: usize, excluded: &[Range<usize>]) -> Vec<Tag> {
    let Some(pattern) = TAG_PATTERN.as_ref() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for captures in pattern.captures_iter(inline_text) {
        let Some(name_match) = captures.get(1) else {
            continue;
        };
        let name = name_match.as_str().trim_end_matches(['-', '/']);
        // A tag needs something other than digits: `#12` is a number.
        if name.is_empty() || name.chars().all(char::is_numeric) {
            continue;
        }
        let hash = name_match.start() - 1 + offset;
        let range = hash..name_match.start() + name.len() + offset;
        if excluded
            .iter()
            .any(|excluded| excluded.start < range.end && range.start < excluded.end)
        {
            continue;
        }
        found.push(Tag {
            name: name.to_string(),
            range,
        });
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wikilinks(text: &str) -> Vec<Wikilink> {
        find_wikilinks(text, 0, &[])
    }

    fn shown<'a>(text: &'a str, link: &Wikilink) -> &'a str {
        &text[link.visible.clone()]
    }

    #[test]
    fn test_a_plain_link_shows_its_target() {
        let text = "see [[Note]] now";
        let links = wikilinks(text);

        assert_eq!(links.len(), 1);
        assert_eq!(&text[links[0].range.clone()], "[[Note]]");
        assert_eq!(shown(text, &links[0]), "Note");
        assert_eq!(&text[links[0].prefix.clone()], "[[");
        assert_eq!(&text[links[0].suffix.clone()], "]]");
        assert_eq!(links[0].target, "Note");
        assert_eq!(links[0].alias, None);
        assert!(!links[0].is_embed);
    }

    #[test]
    fn test_an_alias_is_what_shows_and_the_target_goes_with_the_brackets() {
        let text = "[[Some Note|the alias]]";
        let links = wikilinks(text);

        assert_eq!(shown(text, &links[0]), "the alias");
        assert_eq!(&text[links[0].prefix.clone()], "[[Some Note|");
        assert_eq!(&text[links[0].suffix.clone()], "]]");
        assert_eq!(links[0].target, "Some Note");
        assert_eq!(links[0].alias.as_deref(), Some("the alias"));
    }

    #[test]
    fn test_headings_and_blocks_stay_in_the_target() {
        let links = wikilinks("[[Note#Heading]] [[Note#^abc123]] [[#Here]]");

        let targets: Vec<&str> = links.iter().map(|link| link.target.as_str()).collect();
        assert_eq!(targets, vec!["Note#Heading", "Note#^abc123", "#Here"]);
    }

    #[test]
    fn test_padding_is_left_with_the_brackets() {
        let text = "[[ Note ]] and [[ A | b ]]";
        let links = wikilinks(text);

        assert_eq!(shown(text, &links[0]), "Note");
        assert_eq!(&text[links[0].prefix.clone()], "[[ ");
        assert_eq!(&text[links[0].suffix.clone()], " ]]");
        assert_eq!(shown(text, &links[1]), "b");
        assert_eq!(&text[links[1].prefix.clone()], "[[ A | ");
        assert_eq!(links[1].target, "A");
    }

    #[test]
    fn test_an_empty_alias_is_no_alias() {
        let text = "[[Note|]]";
        let links = wikilinks(text);

        assert_eq!(shown(text, &links[0]), "Note");
        assert_eq!(&text[links[0].suffix.clone()], "|]]");
        assert_eq!(links[0].alias, None);
    }

    #[test]
    fn test_an_embed_includes_its_bang() {
        let text = "a ![[pic.png]] b";
        let links = wikilinks(text);

        assert!(links[0].is_embed);
        assert_eq!(&text[links[0].range.clone()], "![[pic.png]]");
        assert_eq!(&text[links[0].prefix.clone()], "![[");
    }

    #[test]
    fn test_what_is_not_a_link_is_skipped() {
        assert!(wikilinks("[[]]").is_empty());
        assert!(wikilinks("[[ ]]").is_empty());
        assert!(wikilinks("[[|alias]]").is_empty());
        assert!(wikilinks("[[two\nlines]]").is_empty());
        assert!(wikilinks("[[unclosed").is_empty());

        let nested = wikilinks("[[a [[b]]");
        assert_eq!(nested.len(), 1, "the unclosed outer one is not a link");
        assert_eq!(nested[0].target, "b");
    }

    #[test]
    fn test_a_link_inside_an_excluded_range_is_skipped() {
        let text = "`[[a]]` and [[b]]";
        let links = find_wikilinks(text, 0, &[0..7]);

        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "b");
    }

    #[test]
    fn test_ranges_are_shifted_by_the_offset() {
        let links = find_wikilinks("[[a]]", 100, &[]);

        assert_eq!(links[0].range, 100..105);
        assert_eq!(links[0].visible, 102..103);
    }

    fn comment_texts<'a>(text: &'a str, comments: &[Comment]) -> Vec<&'a str> {
        comments
            .iter()
            .map(|comment| &text[comment.range.clone()])
            .collect()
    }

    #[test]
    fn test_a_comment_runs_from_one_double_percent_to_the_next() {
        let text = "a %%hidden%% b %%two%% c";
        let comments = find_comments(text, 0, &[]);

        assert_eq!(
            comment_texts(text, &comments),
            vec!["%%hidden%%", "%%two%%"]
        );
        assert!(comments.iter().all(|comment| !comment.is_multiline));
    }

    #[test]
    fn test_a_comment_may_run_over_lines() {
        let text = "a %%one\ntwo%% b";
        let comments = find_comments(text, 0, &[]);

        assert_eq!(comment_texts(text, &comments), vec!["%%one\ntwo%%"]);
        assert!(comments[0].is_multiline);
    }

    #[test]
    fn test_empty_and_unclosed_comments_are_not_comments() {
        assert!(find_comments("a %%%% b", 0, &[]).is_empty());
        assert!(find_comments("a %%never closed", 0, &[]).is_empty());
        assert!(find_comments("100% and 5%", 0, &[]).is_empty());
    }

    #[test]
    fn test_a_comment_starting_in_an_excluded_range_is_skipped() {
        let text = "`%%x%%` and %%y%%";
        let comments = find_comments(text, 0, &[0..7]);

        assert_eq!(comment_texts(text, &comments), vec!["%%y%%"]);
        assert_eq!(find_comments("%%a%%", 40, &[])[0].range, 40..45);
    }

    #[test]
    fn test_a_block_id_ends_a_line() {
        let text = "a paragraph ^abc-123\nnext line\n^alone\nx^2 and 2^n";
        let ids = find_block_ids(text, 0, &[]);

        let found: Vec<&str> = ids.iter().map(|range| &text[range.clone()]).collect();
        assert_eq!(found, vec!["^abc-123", "^alone"]);
    }

    #[test]
    fn test_a_block_id_must_be_last_and_follow_whitespace() {
        assert!(find_block_ids("a ^id and more", 0, &[]).is_empty());
        assert!(find_block_ids("a^id", 0, &[]).is_empty());
        assert!(find_block_ids("a ^not valid", 0, &[]).is_empty());
        assert_eq!(find_block_ids("trailing ^id   ", 10, &[]), vec![19..22]);
    }

    #[test]
    fn test_a_block_id_inside_an_excluded_range_is_skipped() {
        assert!(find_block_ids("`code ^id`", 0, &[0..10]).is_empty());
    }

    fn tags(text: &str) -> Vec<(String, Range<usize>)> {
        find_tags(text, 0, &[])
            .into_iter()
            .map(|tag| (tag.name, tag.range))
            .collect()
    }

    #[test]
    fn test_tags_start_a_word_and_may_nest() {
        assert_eq!(
            tags("#one and #two/three, (#four) x#no"),
            vec![
                ("one".to_string(), 0..4),
                ("two/three".to_string(), 9..19),
                ("four".to_string(), 22..27),
            ]
        );
    }

    #[test]
    fn test_a_number_is_not_a_tag_and_trailing_dashes_are_not_in_one() {
        assert_eq!(
            tags("#12 #1a #a- #b/"),
            vec![
                ("1a".to_string(), 4..7),
                ("a".to_string(), 8..10),
                ("b".to_string(), 12..14),
            ]
        );
    }

    #[test]
    fn test_a_tag_overlapping_an_excluded_range_is_skipped() {
        let text = "[x](#anchor) #real";
        let found = find_tags(text, 0, &[0..12]);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "real");
    }
}

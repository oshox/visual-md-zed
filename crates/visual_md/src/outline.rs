//! The structure of a Markdown document: its headings, links, tags, tasks and
//! front matter, as the extensions that subscribed to document events see it.
//!
//! Like [`crate::plan`] this works on text and a parse, not on an editor, so it
//! can run on a background thread.

use std::ops::Range;
use std::sync::LazyLock;

use extension::{
    VisualMdLinkStyle, VisualMdOutline, VisualMdOutlineHeading, VisualMdOutlineLink,
    VisualMdOutlineTag, VisualMdOutlineTask,
};
use regex::Regex;
use tree_sitter::{Node, Parser, Tree};

/// The most entries of each kind an outline holds, so that a huge or odd
/// document cannot make an event as large as the document itself.
pub const MAX_ENTRIES_PER_KIND: usize = 5_000;

/// Documents larger than this are not outlined at all.
pub const MAX_DOCUMENT_BYTES: usize = 2 * 1024 * 1024;

/// Builds the outline of `text`, whose block structure is `block_tree`.
pub fn outline(text: &str, block_tree: &Tree) -> VisualMdOutline {
    let mut outline = VisualMdOutline::default();
    let Some(mut inline_parser) = inline_parser() else {
        return outline;
    };
    walk(
        block_tree.root_node(),
        text,
        &mut inline_parser,
        &mut outline,
    );

    outline.headings.sort_by_key(|heading| heading.range.start);
    outline.links.sort_by_key(|link| link.range.start);
    outline.tags.sort_by_key(|tag| tag.range.start);
    outline.tasks.sort_by_key(|task| task.range.start);
    outline.headings.truncate(MAX_ENTRIES_PER_KIND);
    outline.links.truncate(MAX_ENTRIES_PER_KIND);
    outline.tags.truncate(MAX_ENTRIES_PER_KIND);
    outline.tasks.truncate(MAX_ENTRIES_PER_KIND);
    outline
}

fn inline_parser() -> Option<Parser> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_md::INLINE_LANGUAGE.into())
        .ok()?;
    Some(parser)
}

fn walk(node: Node, text: &str, inline_parser: &mut Parser, outline: &mut VisualMdOutline) {
    match node.kind() {
        "fenced_code_block" | "indented_code_block" | "html_block" => return,
        "atx_heading" => atx_heading(node, text, outline),
        "setext_heading" => setext_heading(node, text, outline),
        "task_list_marker_checked" | "task_list_marker_unchecked" => {
            task(node, text, outline);
        }
        "minus_metadata" | "plus_metadata" => {
            outline.frontmatter = frontmatter(node, text);
            return;
        }
        "inline" => {
            inline(node, text, inline_parser, outline);
            return;
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, text, inline_parser, outline);
    }
}

/// `range` without its trailing line break.
fn without_line_break(text: &str, range: Range<usize>) -> Range<usize> {
    let kept = text
        .get(range.clone())
        .map_or(0, |slice| slice.trim_end_matches(['\n', '\r']).len());
    range.start..range.start + kept
}

fn atx_heading(node: Node, text: &str, outline: &mut VisualMdOutline) {
    let level = {
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .find_map(|child| match child.kind() {
                "atx_h1_marker" => Some(1),
                "atx_h2_marker" => Some(2),
                "atx_h3_marker" => Some(3),
                "atx_h4_marker" => Some(4),
                "atx_h5_marker" => Some(5),
                "atx_h6_marker" => Some(6),
                _ => None,
            })
    };
    let Some(level) = level else {
        return;
    };
    let content = node
        .child_by_field_name("heading_content")
        .and_then(|content| text.get(content.byte_range()))
        .unwrap_or_default();
    // A closing run of `#`s, set off by a space, is not part of the heading.
    let trimmed = content.trim_end();
    let without_closing = trimmed.trim_end_matches('#');
    let heading_text =
        if without_closing.is_empty() || without_closing.ends_with(char::is_whitespace) {
            without_closing.trim()
        } else {
            trimmed
        };
    outline.headings.push(VisualMdOutlineHeading {
        level,
        text: heading_text.to_string(),
        range: without_line_break(text, node.byte_range()),
    });
}

fn setext_heading(node: Node, text: &str, outline: &mut VisualMdOutline) {
    let mut level = None;
    let mut content = None;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "setext_h1_underline" => level = Some(1),
            "setext_h2_underline" => level = Some(2),
            "paragraph" => content = text.get(child.byte_range()),
            _ => {}
        }
    }
    let (Some(level), Some(content)) = (level, content) else {
        return;
    };
    outline.headings.push(VisualMdOutlineHeading {
        level,
        text: content.split_whitespace().collect::<Vec<_>>().join(" "),
        range: without_line_break(text, node.byte_range()),
    });
}

fn task(node: Node, text: &str, outline: &mut VisualMdOutline) {
    let range = node.byte_range();
    let line_end = text
        .get(range.end..)
        .and_then(|rest| rest.find('\n'))
        .map_or(text.len(), |newline| range.end + newline);
    let item_text = text.get(range.end..line_end).unwrap_or_default().trim();
    outline.tasks.push(VisualMdOutlineTask {
        text: item_text.to_string(),
        checked: node.kind() == "task_list_marker_checked",
        range,
    });
}

/// The text between the delimiters of a front matter block.
fn frontmatter(node: Node, text: &str) -> Option<String> {
    let block = text.get(node.byte_range())?;
    let mut lines = block.lines();
    lines.next()?;
    let mut body: Vec<&str> = lines.collect();
    body.pop()?;
    Some(body.join("\n"))
}

/// Everything in one `inline` node: its links, embeds and tags. The text is
/// parsed again as inline Markdown with the quote markers of continuation
/// lines blanked, the same way the planner does, since the inline grammar takes
/// a stray `>` for text.
fn inline(node: Node, text: &str, inline_parser: &mut Parser, outline: &mut VisualMdOutline) {
    let range = node.byte_range();
    let Some(original) = text.get(range.clone()) else {
        return;
    };
    let mut bytes = original.as_bytes().to_vec();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "block_continuation" {
            continue;
        }
        let child_range = child.byte_range();
        if let Some(slice) = bytes.get_mut(
            child_range.start.saturating_sub(range.start)
                ..child_range.end.saturating_sub(range.start),
        ) {
            slice.fill(b' ');
        }
    }
    let Ok(inline_text) = String::from_utf8(bytes) else {
        return;
    };
    let Some(tree) = inline_parser.parse(&inline_text, None) else {
        return;
    };

    // Where tags cannot be: code, and the targets of links.
    let mut excluded = Vec::new();
    inline_nodes(
        tree.root_node(),
        &inline_text,
        range.start,
        &mut excluded,
        outline,
    );
    wikilinks(&inline_text, range.start, &mut excluded, outline);
    tags(&inline_text, range.start, &excluded, outline);
}

fn inline_nodes(
    node: Node,
    inline_text: &str,
    offset: usize,
    excluded: &mut Vec<Range<usize>>,
    outline: &mut VisualMdOutline,
) {
    let shifted = |range: Range<usize>| range.start + offset..range.end + offset;
    match node.kind() {
        "code_span" => {
            excluded.push(shifted(node.byte_range()));
            return;
        }
        "inline_link" | "image" => {
            let mut link_text = None;
            let mut destination = None;
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                match child.kind() {
                    "link_text" | "image_description" => {
                        link_text = inline_text.get(child.byte_range())
                    }
                    "link_destination" => destination = inline_text.get(child.byte_range()),
                    _ => {}
                }
            }
            if let Some(destination) = destination {
                // Without a destination it is not a link, and what looks like
                // an image may be the `!` of an embed such as `![[name]]`.
                excluded.push(shifted(node.byte_range()));
                let destination = destination
                    .strip_prefix('<')
                    .and_then(|inner| inner.strip_suffix('>'))
                    .unwrap_or(destination);
                outline.links.push(VisualMdOutlineLink {
                    style: if node.kind() == "image" {
                        VisualMdLinkStyle::Embed
                    } else {
                        VisualMdLinkStyle::Inline
                    },
                    target: destination.to_string(),
                    text: link_text
                        .map(str::trim)
                        .filter(|link_text| !link_text.is_empty())
                        .map(str::to_string),
                    range: shifted(node.byte_range()),
                });
            }
            return;
        }
        "uri_autolink" | "email_autolink" => {
            excluded.push(shifted(node.byte_range()));
            let address = inline_text
                .get(node.byte_range())
                .map(|autolink| autolink.trim_start_matches('<').trim_end_matches('>'));
            if let Some(address) = address {
                outline.links.push(VisualMdOutlineLink {
                    style: VisualMdLinkStyle::Inline,
                    target: address.to_string(),
                    text: None,
                    range: shifted(node.byte_range()),
                });
            }
            return;
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        inline_nodes(child, inline_text, offset, excluded, outline);
    }
}

/// Finds `[[name]]`, `[[name|text]]` and the embed form `![[name]]` by scanning
/// the text, since they are not Markdown grammar at all.
fn wikilinks(
    inline_text: &str,
    offset: usize,
    excluded: &mut Vec<Range<usize>>,
    outline: &mut VisualMdOutline,
) {
    let mut search_from = 0;
    while let Some(found) = inline_text
        .get(search_from..)
        .and_then(|rest| rest.find("[["))
    {
        let open = search_from + found;
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
        let (target, alias) = match inner.split_once('|') {
            Some((target, alias)) => (target.trim(), Some(alias.trim())),
            None => (inner.trim(), None),
        };
        if target.is_empty() {
            continue;
        }
        let range = start + offset..inner_end + 2 + offset;
        excluded.push(range.clone());
        outline.links.push(VisualMdOutlineLink {
            style: if is_embed {
                VisualMdLinkStyle::Embed
            } else {
                VisualMdLinkStyle::Wikilink
            },
            target: target.to_string(),
            text: alias.filter(|alias| !alias.is_empty()).map(str::to_string),
            range,
        });
        search_from = inner_end + 2;
    }
}

/// A `#` at the start of a word, then the name.
static TAG_PATTERN: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(?:^|[\s(\[{,;])#([\p{L}\p{N}_][\p{L}\p{N}_/-]*)").ok());

fn tags(
    inline_text: &str,
    offset: usize,
    excluded: &[Range<usize>],
    outline: &mut VisualMdOutline,
) {
    let Some(pattern) = TAG_PATTERN.as_ref() else {
        return;
    };
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
        outline.tags.push(VisualMdOutlineTag {
            name: name.to_string(),
            range,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::parse_blocks;

    fn outline_of(text: &str) -> VisualMdOutline {
        let tree = parse_blocks(text).expect("the document should parse");
        outline(text, &tree)
    }

    fn headings(outline: &VisualMdOutline) -> Vec<(u8, &str)> {
        outline
            .headings
            .iter()
            .map(|heading| (heading.level, heading.text.as_str()))
            .collect()
    }

    #[test]
    fn test_atx_headings_have_a_level_text_and_range() {
        let text = "# One\n\n## Two words ##\n\n###### Six\n";
        let outline = outline_of(text);

        assert_eq!(
            headings(&outline),
            vec![(1, "One"), (2, "Two words"), (6, "Six")]
        );
        assert_eq!(outline.headings[0].range, 0..5);
        assert_eq!(&text[outline.headings[1].range.clone()], "## Two words ##");
    }

    #[test]
    fn test_a_hash_that_is_part_of_the_text_stays() {
        let outline = outline_of("# C#\n\n## see issue #\n");

        assert_eq!(headings(&outline), vec![(1, "C#"), (2, "see issue")]);
    }

    #[test]
    fn test_setext_headings() {
        let text = "Title\n=====\n\nSub\nheading\n---\n";
        let outline = outline_of(text);

        assert_eq!(headings(&outline), vec![(1, "Title"), (2, "Sub heading")]);
        assert_eq!(&text[outline.headings[0].range.clone()], "Title\n=====");
    }

    #[test]
    fn test_headings_inside_code_are_not_headings() {
        let outline = outline_of("```\n# not a heading\n```\n\n    # nor this\n");

        assert!(outline.headings.is_empty());
    }

    #[test]
    fn test_inline_links_images_and_autolinks() {
        let text = "see [the docs](https://x.org/docs) and ![a pic](pic.png) or <https://y.org>\n";
        let outline = outline_of(text);

        let links: Vec<(VisualMdLinkStyle, &str, Option<&str>)> = outline
            .links
            .iter()
            .map(|link| (link.style, link.target.as_str(), link.text.as_deref()))
            .collect();
        assert_eq!(
            links,
            vec![
                (
                    VisualMdLinkStyle::Inline,
                    "https://x.org/docs",
                    Some("the docs")
                ),
                (VisualMdLinkStyle::Embed, "pic.png", Some("a pic")),
                (VisualMdLinkStyle::Inline, "https://y.org", None),
            ]
        );
        assert_eq!(
            &text[outline.links[0].range.clone()],
            "[the docs](https://x.org/docs)"
        );
    }

    #[test]
    fn test_wikilinks_and_embeds() {
        let text = "a [[Note]] b [[Other note|alias]] c ![[pic.png]] d [[Heading#part]]\n";
        let outline = outline_of(text);

        let links: Vec<(VisualMdLinkStyle, &str, Option<&str>, &str)> = outline
            .links
            .iter()
            .map(|link| {
                (
                    link.style,
                    link.target.as_str(),
                    link.text.as_deref(),
                    &text[link.range.clone()],
                )
            })
            .collect();
        assert_eq!(
            links,
            vec![
                (VisualMdLinkStyle::Wikilink, "Note", None, "[[Note]]"),
                (
                    VisualMdLinkStyle::Wikilink,
                    "Other note",
                    Some("alias"),
                    "[[Other note|alias]]"
                ),
                (VisualMdLinkStyle::Embed, "pic.png", None, "![[pic.png]]"),
                (
                    VisualMdLinkStyle::Wikilink,
                    "Heading#part",
                    None,
                    "[[Heading#part]]"
                ),
            ]
        );
    }

    #[test]
    fn test_links_in_code_are_not_links() {
        let outline = outline_of("`[[Note]]` and `[x](y)` and\n\n```\n[[Block]] [a](b)\n```\n");

        assert!(outline.links.is_empty(), "{:?}", outline.links);
    }

    #[test]
    fn test_unfinished_and_empty_wikilinks_are_ignored() {
        let outline = outline_of("[[open and [[]] and [[ ]] and [[a\nb]]\n");

        assert!(outline.links.is_empty(), "{:?}", outline.links);
    }

    #[test]
    fn test_tags() {
        let text = "# Title #top\n\nsome #idea and #nested/tag, (#paren) not#this #123 #v2 end-#\n";
        let outline = outline_of(text);

        let tags: Vec<(&str, &str)> = outline
            .tags
            .iter()
            .map(|tag| (tag.name.as_str(), &text[tag.range.clone()]))
            .collect();
        assert_eq!(
            tags,
            vec![
                ("top", "#top"),
                ("idea", "#idea"),
                ("nested/tag", "#nested/tag"),
                ("paren", "#paren"),
                ("v2", "#v2"),
            ]
        );
    }

    #[test]
    fn test_tags_in_code_and_link_targets_are_not_tags() {
        let outline = outline_of(
            "`#code` and [x](#anchor) and [[Note#heading]] and <https://x.org/#frag> and https://x.org/#frag\n\n```\n#fenced\n```\n",
        );

        assert!(outline.tags.is_empty(), "{:?}", outline.tags);
    }

    #[test]
    fn test_tasks() {
        let text = "- [ ] write it\n- [x] test it\n  - [ ] nested\n- plain\n";
        let outline = outline_of(text);

        let tasks: Vec<(&str, bool, &str)> = outline
            .tasks
            .iter()
            .map(|task| (task.text.as_str(), task.checked, &text[task.range.clone()]))
            .collect();
        assert_eq!(
            tasks,
            vec![
                ("write it", false, "[ ]"),
                ("test it", true, "[x]"),
                ("nested", false, "[ ]"),
            ]
        );
    }

    #[test]
    fn test_front_matter() {
        let outline = outline_of("---\ntitle: x\ntags: [a, b]\n---\n\n# Body\n");

        assert_eq!(
            outline.frontmatter.as_deref(),
            Some("title: x\ntags: [a, b]")
        );
        assert_eq!(headings(&outline), vec![(1, "Body")]);
    }

    #[test]
    fn test_toml_front_matter() {
        let outline = outline_of("+++\na = 1\n+++\n\nbody\n");

        assert_eq!(outline.frontmatter.as_deref(), Some("a = 1"));
    }

    #[test]
    fn test_a_horizontal_rule_later_is_not_front_matter() {
        let outline = outline_of("text\n\n---\ntitle: x\n---\n");

        assert_eq!(outline.frontmatter, None);
    }

    #[test]
    fn test_things_in_quotes_and_lists_are_found() {
        let text = "> a [[Quoted]] and #quoted\n> more [x](y)\n\n- item [[Listed]] #listed\n";
        let outline = outline_of(text);

        assert_eq!(
            outline
                .links
                .iter()
                .map(|link| link.target.as_str())
                .collect::<Vec<_>>(),
            vec!["Quoted", "y", "Listed"]
        );
        assert_eq!(
            outline
                .tags
                .iter()
                .map(|tag| tag.name.as_str())
                .collect::<Vec<_>>(),
            vec!["quoted", "listed"]
        );
        for link in &outline.links {
            assert!(text.is_char_boundary(link.range.start));
            assert!(text.is_char_boundary(link.range.end));
        }
    }

    #[test]
    fn test_multibyte_text_gets_byte_ranges() {
        let text = "é [[Café]] #tëst\n";
        let outline = outline_of(text);

        assert_eq!(&text[outline.links[0].range.clone()], "[[Café]]");
        assert_eq!(outline.tags[0].name, "tëst");
        assert_eq!(&text[outline.tags[0].range.clone()], "#tëst");
    }

    #[test]
    fn test_an_empty_document_has_an_empty_outline() {
        assert_eq!(outline_of(""), VisualMdOutline::default());
        assert_eq!(outline_of("just text\n"), VisualMdOutline::default());
    }

    #[test]
    fn test_each_kind_is_capped() {
        let text = "#t ".repeat(MAX_ENTRIES_PER_KIND + 10);
        let tags = outline_of(&text).tags;

        assert_eq!(tags.len(), MAX_ENTRIES_PER_KIND);
    }

    /// Whatever the document, every range is in bounds, on character
    /// boundaries and in document order, and nothing panics.
    #[test]
    fn test_random_documents_give_valid_ranges() {
        let fragments = [
            "# Heading #tag\n",
            "Setext\n======\n",
            "- [ ] task [[Link]] #t\n",
            "- [x] done ![[pic]]\n",
            "> quote [a](b) #q\n> more\n",
            "`code [[x]] #y` text é\n",
            "```\n# fenced [[x]]\n```\n",
            "[[open\n",
            "[[a|b]] ![c](d) <https://e.org>\n",
            "---\n",
            "\n",
            "日本語 #日本 [[日本]]\n",
            "+++\nx = 1\n+++\n",
        ];
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move |bound: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state as usize) % bound
        };
        for _ in 0..300 {
            let text: String = (0..1 + next(25))
                .map(|_| fragments[next(fragments.len())])
                .collect();
            let outline = outline_of(&text);

            let ranges: Vec<&Range<usize>> = outline
                .headings
                .iter()
                .map(|heading| &heading.range)
                .chain(outline.links.iter().map(|link| &link.range))
                .chain(outline.tags.iter().map(|tag| &tag.range))
                .chain(outline.tasks.iter().map(|task| &task.range))
                .collect();
            for range in ranges {
                assert!(
                    range.start <= range.end && range.end <= text.len(),
                    "{range:?} for {text:?}"
                );
                assert!(text.is_char_boundary(range.start) && text.is_char_boundary(range.end));
            }
            for window in outline.links.windows(2) {
                assert!(window[0].range.start <= window[1].range.start);
            }
            for window in outline.headings.windows(2) {
                assert!(window[0].range.start <= window[1].range.start);
            }
        }
    }
}

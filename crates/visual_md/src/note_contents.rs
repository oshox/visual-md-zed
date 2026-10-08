//! What is inside the notes of a project that completion offers: the headings
//! and block ids of a note after `[[Note#`, and the tags used anywhere after
//! `#`.
//!
//! The functions over text are pure. The tags of the whole project are found by
//! reading its Markdown files in the background, at most [`MAX_FILES`] of them
//! and none larger than [`MAX_FILE_BYTES`], and are kept for [`LIFETIME`].

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{App, AppContext as _, Entity, EntityId, Global, Task};
use project::Project;

use crate::inline_scan;
use crate::outline;
use crate::plan::{Subpath, parse_blocks};

/// The most Markdown files read to find the tags of a project.
pub const MAX_FILES: usize = 2_000;

/// Files larger than this are not read for tags.
pub const MAX_FILE_BYTES: u64 = 256 * 1024;

/// How long the tags found in a project are kept before the files are read again.
pub const LIFETIME: Duration = Duration::from_secs(30);

/// A heading of a note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    pub text: String,
    /// From 1 to 6.
    pub level: u8,
}

/// The headings of `text`, in order.
pub fn headings(text: &str) -> Vec<Heading> {
    let Some(tree) = parse_blocks(text) else {
        return Vec::new();
    };
    outline::outline(text, &tree)
        .headings
        .into_iter()
        .map(|heading| Heading {
            text: heading.text,
            level: heading.level,
        })
        .collect()
}

/// A block of a note that has an id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockId {
    /// The id without the `^`.
    pub id: String,
    /// The start of the line the id ends, without the id.
    pub preview: String,
}

const PREVIEW_CHARACTERS: usize = 60;

/// The `^block-id`s that end lines of `text`, in order.
pub fn block_ids(text: &str) -> Vec<BlockId> {
    inline_scan::find_block_ids(text, 0, &[])
        .into_iter()
        .filter_map(|range| {
            let id = text.get(range.start + 1..range.end)?;
            let line_start = text
                .get(..range.start)?
                .rfind('\n')
                .map_or(0, |index| index + 1);
            let line = text.get(line_start..range.start)?.trim();
            Some(BlockId {
                id: id.to_string(),
                preview: line.chars().take(PREVIEW_CHARACTERS).collect(),
            })
        })
        .collect()
}

/// The text of a note after its front matter.
fn without_front_matter<'a>(text: &'a str, tree: &tree_sitter::Tree) -> &'a str {
    let mut cursor = tree.root_node().walk();
    let end = tree
        .root_node()
        .children(&mut cursor)
        .find(|child| matches!(child.kind(), "minus_metadata" | "plus_metadata"))
        .map_or(0, |front_matter| front_matter.end_byte());
    text.get(end..).unwrap_or(text)
}

/// What `![[Note#...]]` and a preview of `[[Note#...]]` show: the whole note
/// after its front matter, a heading and everything under it up to the next
/// heading of the same or a higher level, or the block a `^id` ends (a list item
/// with its children, or a paragraph, quote, table or code block) without the id.
/// `None` when the heading or block is not there.
pub fn section(text: &str, subpath: Option<&Subpath>) -> Option<String> {
    let tree = parse_blocks(text)?;
    match subpath {
        None => Some(without_front_matter(text, &tree).trim().to_string()),
        Some(Subpath::Heading(name)) => {
            let wanted = name.rsplit('#').next().unwrap_or(name).trim();
            let headings = outline::outline(text, &tree).headings;
            let (index, heading) = headings
                .iter()
                .enumerate()
                .find(|(_, heading)| heading.text.trim().eq_ignore_ascii_case(wanted))?;
            let end = headings[index + 1..]
                .iter()
                .find(|next| next.level <= heading.level)
                .map_or(text.len(), |next| next.range.start);
            text.get(heading.range.start..end)
                .map(|section| section.trim_end().to_string())
        }
        Some(Subpath::Block(id)) => {
            let marker = format!("^{id}");
            let id_range = inline_scan::find_block_ids(text, 0, &[])
                .into_iter()
                .find(|range| text.get(range.clone()) == Some(marker.as_str()))?;
            let mut node = tree
                .root_node()
                .descendant_for_byte_range(id_range.start, id_range.start)?;
            loop {
                let is_block = matches!(
                    node.kind(),
                    "list_item"
                        | "paragraph"
                        | "block_quote"
                        | "pipe_table"
                        | "fenced_code_block"
                        | "atx_heading"
                        | "setext_heading"
                );
                if is_block {
                    // An id on a list item's first line names the item and what is nested in it.
                    if node.kind() == "paragraph"
                        && let Some(parent) = node.parent().filter(|p| p.kind() == "list_item")
                    {
                        node = parent;
                    }
                    break;
                }
                node = node.parent()?;
            }
            let block = text.get(node.byte_range())?;
            let relative = id_range.start.checked_sub(node.start_byte())?;
            let before = block.get(..relative)?.trim_end();
            let after = block.get(relative + marker.len()..)?;
            Some(format!("{before}{after}").trim().to_string())
        }
    }
}

/// `text` as the Markdown to show in a preview or an embed: the markup of
/// note-taking apps that the Markdown renderer knows nothing about is made
/// readable. A `[[Note|alias]]` becomes its visible text, a nested `![[embed]]`
/// is shown by name rather than expanded, `%%comments%%` and the `^id` that ends
/// a line are dropped. Code is left alone. Cut at `max_bytes`, on a line.
pub fn preview_markdown(text: &str, max_bytes: usize) -> (String, bool) {
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    let code = code_ranges(text);

    for link in inline_scan::find_wikilinks(text, 0, &code) {
        let visible = text.get(link.visible.clone()).unwrap_or_default();
        let replacement = if link.is_embed {
            format!("*↳ {visible}*")
        } else {
            visible.to_string()
        };
        edits.push((link.range.clone(), replacement));
    }
    for comment in inline_scan::find_comments(text, 0, &code) {
        edits.push((comment.range.clone(), String::new()));
    }
    for id in inline_scan::find_block_ids(text, 0, &code) {
        // The space before the id goes with it.
        let start = text.get(..id.start).map_or(id.start, |before| {
            before.trim_end_matches([' ', '\t']).len()
        });
        edits.push((start..id.end, String::new()));
    }
    edits.sort_by_key(|(range, _)| range.start);

    let mut result = String::with_capacity(text.len());
    let mut position = 0;
    for (range, replacement) in edits {
        if range.start < position {
            continue;
        }
        result.push_str(text.get(position..range.start).unwrap_or_default());
        result.push_str(&replacement);
        position = range.end;
    }
    result.push_str(text.get(position..).unwrap_or_default());

    if result.len() <= max_bytes {
        return (result, false);
    }
    let mut cut = max_bytes;
    while !result.is_char_boundary(cut) {
        cut -= 1;
    }
    let cut = result[..cut].rfind('\n').unwrap_or(cut);
    result.truncate(cut);
    (result.trim_end().to_string(), true)
}

/// Fenced and indented code blocks, and inline code spans, of `text`.
fn code_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    if let Some(tree) = parse_blocks(text) {
        let mut pending = vec![tree.root_node()];
        while let Some(node) = pending.pop() {
            if matches!(node.kind(), "fenced_code_block" | "indented_code_block") {
                ranges.push(node.byte_range());
                continue;
            }
            let mut cursor = node.walk();
            pending.extend(node.children(&mut cursor));
        }
    }

    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'`' {
            index += 1;
            continue;
        }
        let run = bytes[index..]
            .iter()
            .take_while(|byte| **byte == b'`')
            .count();
        let mut search = index + run;
        let mut closing = None;
        while search < bytes.len() && bytes[search] != b'\n' {
            if bytes[search] == b'`' {
                let candidate = bytes[search..]
                    .iter()
                    .take_while(|byte| **byte == b'`')
                    .count();
                if candidate == run {
                    closing = Some(search + candidate);
                    break;
                }
                search += candidate;
            } else {
                search += 1;
            }
        }
        match closing {
            Some(end) => {
                ranges.push(index..end);
                index = end;
            }
            None => index += run,
        }
    }
    ranges
}

/// The names of the tags in `text`, each once, in order of appearance.
pub fn tags(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    inline_scan::find_tags(text, 0, &[])
        .into_iter()
        .filter(|tag| seen.insert(tag.name.clone()))
        .map(|tag| tag.name)
        .collect()
}

/// How many notes use each tag, the most used first.
pub type TagCounts = Arc<Vec<(String, usize)>>;

fn count_tags<'a>(texts: impl IntoIterator<Item = &'a str>) -> TagCounts {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for text in texts {
        for tag in tags(text) {
            *counts.entry(tag).or_default() += 1;
        }
    }
    let mut counts: Vec<(String, usize)> = counts.into_iter().collect();
    counts.sort_by(|(left_name, left), (right_name, right)| {
        right.cmp(left).then_with(|| left_name.cmp(right_name))
    });
    Arc::new(counts)
}

#[derive(Default)]
struct TagCaches(HashMap<EntityId, (Instant, TagCounts)>);

impl Global for TagCaches {}

/// The tags used in the Markdown files of `project`: what was found within the
/// last [`LIFETIME`], or else what reading the files finds. A project on a
/// remote server is not read.
pub fn project_tags(project: &Entity<Project>, cx: &mut App) -> Task<TagCounts> {
    let project_id = project.entity_id();
    if let Some((scanned, counts)) = cx
        .try_global::<TagCaches>()
        .and_then(|caches| caches.0.get(&project_id))
        && scanned.elapsed() < LIFETIME
    {
        return Task::ready(counts.clone());
    }

    let project_ref = project.read(cx);
    if project_ref.is_via_remote_server() {
        return Task::ready(Arc::default());
    }
    let fs = project_ref.fs().clone();
    let mut paths = Vec::new();
    'worktrees: for worktree in project_ref.visible_worktrees(cx) {
        let snapshot = worktree.read(cx).snapshot();
        for entry in snapshot.files(false, 0) {
            let is_markdown = entry
                .path
                .extension()
                .is_some_and(|extension| matches!(extension, "md" | "markdown"));
            if is_markdown && entry.size <= MAX_FILE_BYTES {
                paths.push(snapshot.absolutize(&entry.path));
                if paths.len() >= MAX_FILES {
                    break 'worktrees;
                }
            }
        }
    }

    cx.spawn(async move |cx| {
        let counts = cx
            .background_spawn(async move {
                let mut texts = Vec::with_capacity(paths.len());
                for path in paths {
                    match fs.load(&path).await {
                        Ok(text) => texts.push(text),
                        Err(error) => {
                            log::debug!("could not read {path:?} for its tags: {error:#}")
                        }
                    }
                }
                count_tags(texts.iter().map(String::as_str))
            })
            .await;
        cx.update(|cx| {
            cx.default_global::<TagCaches>()
                .0
                .insert(project_id, (Instant::now(), counts.clone()));
        });
        counts
    })
}

#[cfg(test)]
mod tests {
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;

    use super::*;

    #[test]
    fn test_headings_come_in_order_with_their_levels() {
        let text = "# One\n\ntext\n\n## Two **bold**\n\n```\n# not a heading\n```\n\n### Three\n";
        let found: Vec<(u8, String)> = headings(text)
            .into_iter()
            .map(|heading| (heading.level, heading.text))
            .collect();
        assert_eq!(
            found,
            [
                (1, "One".to_string()),
                (2, "Two **bold**".to_string()),
                (3, "Three".to_string())
            ]
        );
    }

    #[test]
    fn test_a_document_without_headings_has_none() {
        assert_eq!(headings("just text\n"), []);
        assert_eq!(headings(""), []);
    }

    #[test]
    fn test_block_ids_come_with_a_preview_of_their_line() {
        let text = "A paragraph that is long enough to be cut off somewhere around the sixty-first character ^para\n\nshort ^b-2\nx^2 is not one\n";
        let found = block_ids(text);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].id, "para");
        assert_eq!(found[0].preview.chars().count(), PREVIEW_CHARACTERS);
        assert!(found[0].preview.starts_with("A paragraph that is long"));
        assert_eq!(
            found[1],
            BlockId {
                id: "b-2".to_string(),
                preview: "short".to_string()
            }
        );
    }

    #[test]
    fn test_a_block_id_with_a_multibyte_line_is_cut_on_a_character() {
        let text = format!("{} ^id\n", "é".repeat(100));
        let found = block_ids(&text);
        assert_eq!(found[0].preview, "é".repeat(PREVIEW_CHARACTERS));
    }

    fn heading(name: &str) -> Subpath {
        Subpath::Heading(name.to_string())
    }

    const NOTE: &str = "---\ntitle: x\n---\n# One\nintro\n\n## Sub\nsub text\n\n### Deep\ndeep text\n\n## Next\nnext text ^blk\n\n- item ^li\n  - child\n- other\n\n# Two\ntwo text\n";

    #[test]
    fn test_the_whole_note_is_shown_without_its_front_matter() {
        let whole = section(NOTE, None).expect("a note");
        assert!(whole.starts_with("# One"), "{whole:?}");
        assert!(whole.ends_with("two text"));
        assert!(!whole.contains("title: x"));
    }

    #[test]
    fn test_a_heading_runs_to_the_next_heading_of_its_level_or_higher() {
        let sub = section(NOTE, Some(&heading("Sub"))).expect("found");
        assert_eq!(sub, "## Sub\nsub text\n\n### Deep\ndeep text");

        let one = section(NOTE, Some(&heading("One"))).expect("found");
        assert!(one.contains("### Deep") && one.contains("next text"));
        assert!(!one.contains("two text"));

        let last = section(NOTE, Some(&heading("Two"))).expect("found");
        assert_eq!(last, "# Two\ntwo text");
    }

    #[test]
    fn test_a_heading_is_found_without_regard_to_case_and_by_its_last_part() {
        assert!(section(NOTE, Some(&heading("sub"))).is_some());
        assert!(section(NOTE, Some(&heading("One#Sub"))).is_some());
        assert!(section(NOTE, Some(&heading("Nothing"))).is_none());
    }

    #[test]
    fn test_a_block_id_names_its_paragraph_or_list_item_without_the_id() {
        let paragraph = section(NOTE, Some(&Subpath::Block("blk".to_string()))).expect("found");
        assert_eq!(paragraph, "next text");

        let item = section(NOTE, Some(&Subpath::Block("li".to_string()))).expect("found");
        assert_eq!(item, "- item\n  - child");

        assert!(section(NOTE, Some(&Subpath::Block("missing".to_string()))).is_none());
    }

    #[test]
    fn test_a_note_without_front_matter_and_an_empty_one_are_fine() {
        assert_eq!(section("plain\n", None).as_deref(), Some("plain"));
        assert_eq!(section("", None).as_deref(), Some(""));
        assert_eq!(section("", Some(&heading("x"))), None);
    }

    #[test]
    fn test_a_preview_makes_wikilinks_readable_and_nested_embeds_a_name() {
        let (text, truncated) = preview_markdown(
            "See [[Note|the note]] and [[Other#Part]], ![[Child]] and `[[code]]`. %%secret%% done ^id\n\n```\n[[fenced]]\n```\n",
            10_000,
        );
        assert!(!truncated);
        assert_eq!(
            text,
            "See the note and Other#Part, *↳ Child* and `[[code]]`.  done\n\n```\n[[fenced]]\n```\n"
        );
    }

    #[test]
    fn test_a_long_preview_is_cut_on_a_line_and_says_so() {
        let source = "first line\nsecond line\nthird line\n";
        let (text, truncated) = preview_markdown(source, 15);
        assert!(truncated);
        assert_eq!(text, "first line");

        let multibyte = format!("{}\n{}", "é".repeat(10), "ü".repeat(10));
        let (cut, truncated) = preview_markdown(&multibyte, 25);
        assert!(truncated);
        assert_eq!(cut, "é".repeat(10));
        assert!(!preview_markdown(&multibyte, 1000).1);
    }

    #[test]
    fn test_tags_are_listed_once_each() {
        assert_eq!(
            tags("#a and #b/c and #a again, not #12 nor x#y\n# Heading\n"),
            ["a", "b/c"]
        );
    }

    #[test]
    fn test_tags_are_counted_by_note_and_the_most_used_come_first() {
        let counts = count_tags(["#b #a #a", "#a", "#c #b", "#b"]);
        assert_eq!(
            *counts,
            vec![
                ("b".to_string(), 3),
                ("a".to_string(), 2),
                ("c".to_string(), 1)
            ]
        );
    }

    #[gpui::test]
    async fn test_the_tags_of_a_project_are_read_from_its_markdown_files(cx: &mut TestAppContext) {
        crate::integration_tests::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/dir",
            json!({
                "a.md": "#work and #idea",
                "sub": { "b.markdown": "#work", "c.txt": "#ignored" },
                "big.md": "#big ".repeat(60_000),
            }),
        )
        .await;
        let project = Project::test(fs.clone(), ["/dir".as_ref()], cx).await;
        cx.run_until_parked();

        let counts = cx.update(|cx| project_tags(&project, cx)).await;

        assert_eq!(
            *counts,
            vec![("work".to_string(), 2), ("idea".to_string(), 1)]
        );
    }

    #[gpui::test]
    async fn test_tags_found_a_moment_ago_are_not_looked_for_again(cx: &mut TestAppContext) {
        crate::integration_tests::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/dir", json!({ "a.md": "#first" })).await;
        let project = Project::test(fs.clone(), ["/dir".as_ref()], cx).await;
        cx.run_until_parked();
        let first = cx.update(|cx| project_tags(&project, cx)).await;

        fs.insert_file("/dir/a.md", b"#second".to_vec()).await;
        cx.run_until_parked();
        let again = cx.update(|cx| project_tags(&project, cx)).await;

        assert_eq!(*first, vec![("first".to_string(), 1)]);
        assert_eq!(*again, *first);
    }
}

//! What is inside the notes of a project that completion offers: the headings
//! and block ids of a note after `[[Note#`, and the tags used anywhere after
//! `#`.
//!
//! The functions over text are pure. The tags of the whole project are found by
//! reading its Markdown files in the background, at most [`MAX_FILES`] of them
//! and none larger than [`MAX_FILE_BYTES`], and are kept for [`LIFETIME`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{App, AppContext as _, Entity, EntityId, Global, Task};
use project::Project;

use crate::inline_scan;
use crate::outline;
use crate::plan::parse_blocks;

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

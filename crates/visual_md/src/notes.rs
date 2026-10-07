//! Which notes a project has: what `[[Note]]` names, whether a file by that name
//! exists, and which one it is.
//!
//! This answers by file name, the way note-taking apps do: `[[Note]]` is
//! `Note.md` wherever it is in the project, `[[folder/Note]]` is the one in a
//! folder of that name, and when several match the closest to the note being
//! edited wins, then the shortest path. Anything inside the files (headings,
//! block ids, aliases in front matter) is for extensions to answer.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::{
    App, AppContext as _, Context, Entity, EntityId, Global, Subscription, Task, WeakEntity,
};
use project::{PathChange, Project, ProjectPath, WorktreeId};
use util::rel_path::RelPath;

const MARKDOWN_EXTENSIONS: [&str; 2] = ["md", "markdown"];

/// The files `![[name]]` can embed besides notes.
const IMAGE_EXTENSIONS: [&str; 8] = ["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "avif"];

/// A file a `[[` can name, and what goes between the brackets to name it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteSuggestion {
    /// A note's name without its extension, or an image's with it, and with
    /// its folders when another file would have the same name.
    pub name: String,
    /// The path of the file in its worktree.
    pub path: String,
}

/// A file of the project that a link can name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteFile {
    pub worktree_id: WorktreeId,
    pub path: Arc<RelPath>,
    /// The path in lowercase, without the extension for a Markdown file.
    lowered: Arc<str>,
}

impl NoteFile {
    pub fn project_path(&self) -> ProjectPath {
        ProjectPath {
            worktree_id: self.worktree_id,
            path: self.path.clone(),
        }
    }
}

/// What a link names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    Found(NoteFile),
    /// The project has no such file.
    Missing,
    /// There is no telling, because the project is still being read, or the
    /// note being edited is not in a worktree the index covers.
    Unknown,
}

#[derive(Default, PartialEq, Eq)]
struct Entries {
    /// By lowercased file name, with its extension, for every file.
    by_name: HashMap<String, Vec<NoteFile>>,
    /// By lowercased file name without its extension, for Markdown files.
    by_stem: HashMap<String, Vec<NoteFile>>,
    worktrees: HashSet<WorktreeId>,
    /// Whether every worktree has been read to the end.
    ready: bool,
}

fn is_markdown(extension: Option<&str>) -> bool {
    extension.is_some_and(|extension| {
        MARKDOWN_EXTENSIONS
            .iter()
            .any(|markdown| extension.eq_ignore_ascii_case(markdown))
    })
}

/// A path's file name, split into its stem and extension. A name that is only a
/// dot and a word, such as `.gitignore`, has no extension.
fn split_name(name: &str) -> (&str, Option<&str>) {
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => {
            (stem, Some(extension))
        }
        _ => (name, None),
    }
}

impl Entries {
    fn build(
        files: &[(WorktreeId, Arc<RelPath>)],
        worktrees: HashSet<WorktreeId>,
        ready: bool,
    ) -> Self {
        let mut entries = Entries {
            worktrees,
            ready,
            ..Default::default()
        };
        for (worktree_id, path) in files {
            let unix = path.as_unix_str();
            let name = unix.rsplit('/').next().unwrap_or(unix);
            let (stem, extension) = split_name(name);
            let markdown = is_markdown(extension);
            let lowered_path = unix.to_lowercase();
            let lowered: Arc<str> = if markdown {
                extension
                    .and_then(|extension| {
                        lowered_path.get(..lowered_path.len() - extension.len() - 1)
                    })
                    .unwrap_or(&lowered_path)
                    .into()
            } else {
                lowered_path.as_str().into()
            };
            let file = NoteFile {
                worktree_id: *worktree_id,
                path: path.clone(),
                lowered,
            };
            entries
                .by_name
                .entry(name.to_lowercase())
                .or_default()
                .push(file.clone());
            if markdown {
                entries
                    .by_stem
                    .entry(stem.to_lowercase())
                    .or_default()
                    .push(file);
            }
        }
        entries
    }

    /// The notes, and with `embed` the images too, that a `[[` can name, the
    /// closest to `from` first.
    fn suggestions(&self, from: Option<&ProjectPath>, embed: bool) -> Vec<NoteSuggestion> {
        let mut files: Vec<(&NoteFile, String)> = Vec::new();
        for (stem, notes) in &self.by_stem {
            let ambiguous = notes.len() > 1;
            for note in notes {
                let unix = note.path.as_unix_str();
                let without_extension = split_name(unix).0;
                let name = if ambiguous {
                    without_extension
                } else {
                    without_extension.rsplit('/').next().unwrap_or(stem)
                };
                files.push((note, name.to_string()));
            }
        }
        if embed {
            for (name, images) in &self.by_name {
                let is_image = split_name(name).1.is_some_and(|extension| {
                    IMAGE_EXTENSIONS
                        .iter()
                        .any(|image| extension.eq_ignore_ascii_case(image))
                });
                if !is_image {
                    continue;
                }
                let ambiguous = images.len() > 1;
                for image in images {
                    let unix = image.path.as_unix_str();
                    let shown = if ambiguous {
                        unix
                    } else {
                        unix.rsplit('/').next().unwrap_or(unix)
                    };
                    files.push((image, shown.to_string()));
                }
            }
        }
        files.sort_by(|(left, left_name), (right, right_name)| {
            closeness(left, from)
                .cmp(&closeness(right, from))
                .then_with(|| left_name.cmp(right_name))
        });
        files
            .into_iter()
            .map(|(file, name)| NoteSuggestion {
                name,
                path: file.path.as_unix_str().to_string(),
            })
            .collect()
    }

    fn resolve(&self, note: &str, from: Option<&ProjectPath>) -> Resolution {
        let note = note.trim().trim_start_matches('/');
        if note.is_empty() || !self.ready {
            return Resolution::Unknown;
        }
        if from.is_some_and(|from| !self.worktrees.contains(&from.worktree_id)) {
            return Resolution::Unknown;
        }

        let lowered = note.to_lowercase().replace('\\', "/");
        let name = lowered.rsplit('/').next().unwrap_or(&lowered);
        let has_directory = lowered.len() > name.len();
        let without_markdown_extension = MARKDOWN_EXTENSIONS
            .iter()
            .find_map(|extension| lowered.strip_suffix(&format!(".{extension}")))
            .unwrap_or(&lowered);

        // A name is looked for as a Markdown file first, so that `[[Notes 1.2]]`
        // finds `Notes 1.2.md` and not nothing for want of an extension.
        let named_like_a_note =
            self.by_stem.get(name).into_iter().flatten().filter(|file| {
                in_directory(&file.lowered, without_markdown_extension, has_directory)
            });
        let named_exactly = self.by_name.get(name).into_iter().flatten().filter(|file| {
            in_directory(
                &file.path.as_unix_str().to_lowercase(),
                &lowered,
                has_directory,
            )
        });

        let best = named_like_a_note
            .chain(named_exactly)
            .min_by_key(|file| closeness(file, from));
        match best {
            Some(file) => Resolution::Found(file.clone()),
            None => Resolution::Missing,
        }
    }
}

/// Whether `candidate` is `wanted`, or ends with it after a `/`, when `wanted`
/// names a directory at all. A bare name is in any directory.
fn in_directory(candidate: &str, wanted: &str, has_directory: bool) -> bool {
    if !has_directory {
        return true;
    }
    candidate == wanted
        || candidate
            .strip_suffix(wanted)
            .is_some_and(|before| before.ends_with('/'))
}

/// Smaller is closer: the same folder as the note being edited, then the same
/// worktree, then the fewest folders, then alphabetical so that the answer
/// never depends on the order the files were found in.
fn closeness<'a>(file: &'a NoteFile, from: Option<&ProjectPath>) -> (u8, usize, &'a str) {
    let place = from.map_or(2, |from| {
        let same_folder =
            file.worktree_id == from.worktree_id && file.path.parent() == from.path.parent();
        match (same_folder, file.worktree_id == from.worktree_id) {
            (true, _) => 0,
            (false, true) => 1,
            _ => 2,
        }
    });
    let depth = file.path.as_unix_str().matches('/').count();
    (place, depth, file.path.as_unix_str())
}

/// Where the file a buffer is of sits in its project, if it is of one.
pub fn location_of(buffer: &language::Buffer, cx: &App) -> Option<ProjectPath> {
    let file = buffer.file()?;
    Some(ProjectPath {
        worktree_id: file.worktree_id(cx),
        path: file.path().clone(),
    })
}

/// The notes of one project, kept current as its worktrees change.
pub struct NoteIndex {
    entries: Entries,
    project: WeakEntity<Project>,
    rebuilding: Option<Task<()>>,
    _subscription: Subscription,
}

#[derive(Default)]
struct NoteIndexes(HashMap<EntityId, Entity<NoteIndex>>);

impl Global for NoteIndexes {}

impl NoteIndex {
    /// The index of `project`, shared by every editor of it.
    pub fn for_project(project: &Entity<Project>, cx: &mut App) -> Entity<Self> {
        let project_id = project.entity_id();
        if let Some(index) = cx
            .try_global::<NoteIndexes>()
            .and_then(|indexes| indexes.0.get(&project_id))
        {
            return index.clone();
        }

        let index = cx.new(|cx| Self::new(project, cx));
        cx.default_global::<NoteIndexes>()
            .0
            .insert(project_id, index.clone());
        cx.observe_release(project, move |_, cx| {
            cx.default_global::<NoteIndexes>().0.remove(&project_id);
        })
        .detach();
        index
    }

    fn new(project: &Entity<Project>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(project, |this, _, event, cx| match event {
            project::Event::WorktreeAdded(_) | project::Event::WorktreeRemoved(_) => {
                this.rebuild(cx)
            }
            project::Event::WorktreeUpdatedEntries(_, changes) => {
                let any_file_came_or_went = changes
                    .iter()
                    .any(|(_, _, change)| !matches!(change, PathChange::Updated));
                if any_file_came_or_went || this.finished_reading(cx) != this.entries.ready {
                    this.rebuild(cx);
                }
            }
            _ => {}
        });
        let mut index = Self {
            entries: Entries::default(),
            project: project.downgrade(),
            rebuilding: None,
            _subscription: subscription,
        };
        index.rebuild(cx);
        index
    }

    /// Whether every visible worktree has been read to the end.
    fn finished_reading(&self, cx: &App) -> bool {
        self.project.upgrade().is_some_and(|project| {
            project
                .read(cx)
                .visible_worktrees(cx)
                .all(|worktree| worktree.read(cx).completed_scan_id() > 0)
        })
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.project.upgrade() else {
            return;
        };
        let ready = self.finished_reading(cx);
        let mut worktrees = HashSet::new();
        let mut files = Vec::new();
        for worktree in project.read(cx).visible_worktrees(cx) {
            let worktree = worktree.read(cx);
            worktrees.insert(worktree.id());
            let worktree_id = worktree.id();
            files.extend(
                worktree
                    .snapshot()
                    .files(false, 0)
                    .map(|entry| (worktree_id, entry.path.clone())),
            );
        }

        self.rebuilding = Some(cx.spawn(async move |this, cx| {
            let entries = cx
                .background_spawn(async move { Entries::build(&files, worktrees, ready) })
                .await;
            this.update(cx, |this, cx| {
                if this.entries != entries {
                    this.entries = entries;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// What `note` names, from the point of view of the note at `from`.
    pub fn resolve(&self, note: &str, from: Option<&ProjectPath>) -> Resolution {
        self.entries.resolve(note, from)
    }

    /// What a `[[` (or with `embed`, a `![[`) can name, the closest to `from`
    /// first. Empty until the project has been read.
    pub fn suggestions(&self, from: Option<&ProjectPath>, embed: bool) -> Vec<NoteSuggestion> {
        self.entries.suggestions(from, embed)
    }
}

#[cfg(test)]
mod tests {
    use fs::{FakeFs, Fs as _};
    use gpui::TestAppContext;
    use serde_json::json;
    use util::rel_path::rel_path;

    use super::*;

    fn worktree() -> WorktreeId {
        WorktreeId::from_usize(1)
    }

    fn entries(paths: &[&str]) -> Entries {
        let files: Vec<(WorktreeId, Arc<RelPath>)> = paths
            .iter()
            .map(|path| (worktree(), Arc::from(rel_path(path))))
            .collect();
        Entries::build(&files, HashSet::from([worktree()]), true)
    }

    fn from(path: &str) -> ProjectPath {
        ProjectPath {
            worktree_id: worktree(),
            path: Arc::from(rel_path(path)),
        }
    }

    fn found(resolution: Resolution) -> Option<String> {
        match resolution {
            Resolution::Found(file) => Some(file.path.as_unix_str().to_string()),
            _ => None,
        }
    }

    fn names(suggestions: Vec<NoteSuggestion>) -> Vec<String> {
        suggestions
            .into_iter()
            .map(|suggestion| suggestion.name)
            .collect()
    }

    #[test]
    fn test_a_note_is_suggested_by_its_name_and_by_its_folders_only_when_it_must_be() {
        let index = entries(&[
            "Inbox.md",
            "projects/Plan.md",
            "archive/Plan.md",
            "projects/deep/Idea.markdown",
            "Version 1.2 notes.md",
            "notes.txt",
        ]);

        assert_eq!(
            names(index.suggestions(None, false)),
            [
                "Inbox",
                "Version 1.2 notes",
                "archive/Plan",
                "projects/Plan",
                "Idea",
            ]
        );
    }

    #[test]
    fn test_images_are_suggested_only_for_an_embed_and_with_their_extension() {
        let index = entries(&[
            "Note.md",
            "pic.png",
            "photos/pic.PNG",
            "doc.pdf",
            "photos/sun.jpg",
        ]);

        assert_eq!(names(index.suggestions(None, false)), ["Note"]);
        assert_eq!(
            names(index.suggestions(None, true)),
            ["Note", "pic.png", "photos/pic.PNG", "sun.jpg"]
        );
    }

    #[test]
    fn test_suggestions_start_with_the_notes_closest_to_the_one_being_edited() {
        let index = entries(&["a/Far.md", "b/Near.md", "b/Also.md", "Top.md"]);

        assert_eq!(
            names(index.suggestions(Some(&from("b/Current.md")), false)),
            ["Also", "Near", "Top", "Far"]
        );
    }

    #[test]
    fn test_a_suggestion_says_which_file_it_names() {
        let index = entries(&["projects/Plan.md"]);

        assert_eq!(
            index.suggestions(None, false),
            [NoteSuggestion {
                name: "Plan".to_string(),
                path: "projects/Plan.md".to_string(),
            }]
        );
    }

    #[test]
    fn test_a_name_finds_the_note_wherever_it_is_whatever_the_case() {
        let index = entries(&[
            "Inbox.md",
            "projects/Plan.md",
            "projects/deep/Idea.markdown",
        ]);

        assert_eq!(
            found(index.resolve("plan", None)).as_deref(),
            Some("projects/Plan.md")
        );
        assert_eq!(
            found(index.resolve("INBOX", None)).as_deref(),
            Some("Inbox.md")
        );
        assert_eq!(
            found(index.resolve("idea", None)).as_deref(),
            Some("projects/deep/Idea.markdown")
        );
        assert_eq!(index.resolve("nothing", None), Resolution::Missing);
    }

    #[test]
    fn test_the_extension_is_optional_and_other_files_need_theirs() {
        let index = entries(&["Plan.md", "diagram.png", "Version 1.2 notes.md"]);

        assert_eq!(
            found(index.resolve("Plan.md", None)).as_deref(),
            Some("Plan.md")
        );
        assert_eq!(
            found(index.resolve("Plan", None)).as_deref(),
            Some("Plan.md")
        );
        assert_eq!(
            found(index.resolve("diagram.png", None)).as_deref(),
            Some("diagram.png")
        );
        assert_eq!(index.resolve("diagram", None), Resolution::Missing);
        assert_eq!(
            found(index.resolve("Version 1.2 notes", None)).as_deref(),
            Some("Version 1.2 notes.md"),
            "a dot in the name is not an extension"
        );
    }

    #[test]
    fn test_a_folder_in_the_link_picks_the_note_in_that_folder() {
        let index = entries(&["a/Note.md", "b/Note.md", "b/inner/Note.md"]);

        assert_eq!(
            found(index.resolve("b/Note", None)).as_deref(),
            Some("b/Note.md")
        );
        assert_eq!(
            found(index.resolve("inner/Note", None)).as_deref(),
            Some("b/inner/Note.md")
        );
        assert_eq!(
            found(index.resolve("b/inner/Note.md", None)).as_deref(),
            Some("b/inner/Note.md")
        );
        assert_eq!(index.resolve("c/Note", None), Resolution::Missing);
        assert_eq!(
            index.resolve("nner/Note", None),
            Resolution::Missing,
            "a folder name matches whole, not by its ending"
        );
    }

    #[test]
    fn test_the_closest_note_wins_then_the_shallowest() {
        let index = entries(&["x/Note.md", "y/z/Note.md", "y/Note.md"]);

        assert_eq!(
            found(index.resolve("Note", Some(&from("y/z/other.md")))).as_deref(),
            Some("y/z/Note.md"),
            "the same folder"
        );
        assert_eq!(
            found(index.resolve("Note", Some(&from("elsewhere.md")))).as_deref(),
            Some("x/Note.md"),
            "from nowhere in particular, the shortest path, and then the first in order"
        );
        assert_eq!(
            found(index.resolve("Note", None)).as_deref(),
            Some("x/Note.md")
        );
    }

    #[test]
    fn test_a_note_in_another_worktree_is_found_when_this_one_has_none() {
        let other = WorktreeId::from_usize(2);
        let files = vec![(other, Arc::from(rel_path("Remote.md")))];
        let index = Entries::build(&files, HashSet::from([worktree(), other]), true);

        let Resolution::Found(file) = index.resolve("remote", Some(&from("here.md"))) else {
            panic!("the other worktree's note is found");
        };
        assert_eq!(file.worktree_id, other);
    }

    #[test]
    fn test_nothing_is_known_until_the_project_is_read_or_for_a_note_outside_it() {
        let files = vec![(worktree(), Arc::from(rel_path("Plan.md")))];
        let unfinished = Entries::build(&files, HashSet::from([worktree()]), false);
        assert_eq!(unfinished.resolve("Plan", None), Resolution::Unknown);

        let finished = Entries::build(&files, HashSet::from([worktree()]), true);
        let outside = ProjectPath {
            worktree_id: WorktreeId::from_usize(9),
            path: Arc::from(rel_path("lone.md")),
        };
        assert_eq!(
            finished.resolve("Plan", Some(&outside)),
            Resolution::Unknown
        );
        assert_eq!(finished.resolve("", None), Resolution::Unknown);
        assert_eq!(finished.resolve("  ", None), Resolution::Unknown);
    }

    #[test]
    fn test_a_file_with_a_leading_dot_has_no_extension() {
        assert_eq!(split_name(".gitignore"), (".gitignore", None));
        assert_eq!(split_name("a.b.md"), ("a.b", Some("md")));
        assert_eq!(split_name("noextension"), ("noextension", None));
        assert_eq!(split_name("trailing."), ("trailing.", None));
    }

    async fn project_with(
        cx: &mut TestAppContext,
        tree: serde_json::Value,
    ) -> (Arc<FakeFs>, Entity<Project>) {
        crate::integration_tests::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/dir", tree).await;
        let project = Project::test(fs.clone(), ["/dir".as_ref()], cx).await;
        (fs, project)
    }

    fn resolves(cx: &mut TestAppContext, index: &Entity<NoteIndex>, note: &str) -> Option<String> {
        cx.read(|cx| found(index.read(cx).resolve(note, None)))
    }

    #[gpui::test]
    async fn test_the_index_follows_the_files_of_the_project(cx: &mut TestAppContext) {
        let (fs, project) =
            project_with(cx, json!({ "Plan.md": "", "sub": { "Idea.md": "" } })).await;
        let index = cx.update(|cx| NoteIndex::for_project(&project, cx));
        cx.run_until_parked();

        assert_eq!(resolves(cx, &index, "plan").as_deref(), Some("Plan.md"));
        assert_eq!(resolves(cx, &index, "idea").as_deref(), Some("sub/Idea.md"));
        assert_eq!(resolves(cx, &index, "new"), None);

        fs.insert_file("/dir/New.md", Vec::new()).await;
        cx.run_until_parked();
        assert_eq!(resolves(cx, &index, "new").as_deref(), Some("New.md"));

        fs.remove_file("/dir/Plan.md".as_ref(), Default::default())
            .await
            .expect("the file is removed");
        cx.run_until_parked();
        assert_eq!(resolves(cx, &index, "plan"), None);
    }

    #[gpui::test]
    async fn test_every_editor_of_a_project_shares_one_index(cx: &mut TestAppContext) {
        let (_fs, project) = project_with(cx, json!({})).await;

        let first = cx.update(|cx| NoteIndex::for_project(&project, cx));
        let second = cx.update(|cx| NoteIndex::for_project(&project, cx));

        assert_eq!(first.entity_id(), second.entity_id());
    }

    #[gpui::test]
    async fn test_observers_hear_of_new_notes_and_not_of_nothing_new(cx: &mut TestAppContext) {
        let (fs, project) = project_with(cx, json!({ "Plan.md": "" })).await;
        let index = cx.update(|cx| NoteIndex::for_project(&project, cx));
        cx.run_until_parked();

        let heard = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let _subscription = cx.update(|cx| {
            let heard = heard.clone();
            cx.observe(&index, move |_, _| {
                heard.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
        });

        fs.insert_file("/dir/Plan.md", b"changed".to_vec()).await;
        cx.run_until_parked();
        assert_eq!(
            heard.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "editing a note changes no name"
        );

        fs.insert_file("/dir/Other.md", Vec::new()).await;
        cx.run_until_parked();
        assert_eq!(heard.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}

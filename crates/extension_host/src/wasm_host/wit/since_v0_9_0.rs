//! API version 0.9.0 exists only in this fork, for the `visual_md` hooks.
//!
//! Every interface is a byte-for-byte copy of 0.8.0's, so they are remapped
//! onto it instead of generated again. That keeps this file small and keeps it
//! from conflicting with upstream's edits to 0.8.0.

use crate::wasm_host::{WasmState, wit::ToWasmtimeResult};
use ::settings::{Settings as _, WorktreeId};
use anyhow::Result;
use extension::{KeyValueStoreDelegate, ProjectDelegate, WorktreeDelegate};
use futures::FutureExt as _;
use gpui::BackgroundExecutor;
use language::{LanguageName, language_settings::AllLanguageSettings};
use semver::Version;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use util::{paths::PathStyle, rel_path::RelPath};
use wasmtime::component::{Linker, Resource};

use super::latest;

pub const MIN_VERSION: Version = Version::new(0, 9, 0);

wasmtime::component::bindgen!({
    imports: {
        default: async | trappable,
    },
    exports: {
        default: async,
    },
    path: "../extension_api/wit/since_v0.9.0",
    with: {
        "worktree": ExtensionWorktree,
        "project": ExtensionProject,
        "key-value-store": ExtensionKeyValueStore,
        "zed:extension/common": latest::zed::extension::common,
        "zed:extension/context-server": latest::zed::extension::context_server,
        "zed:extension/dap": latest::zed::extension::dap,
        "zed:extension/github": latest::zed::extension::github,
        "zed:extension/http-client": latest::zed::extension::http_client,
        "zed:extension/lsp": latest::zed::extension::lsp,
        "zed:extension/nodejs": latest::zed::extension::nodejs,
        "zed:extension/platform": latest::zed::extension::platform,
        "zed:extension/process": latest::zed::extension::process,
        "zed:extension/slash-command": latest::zed::extension::slash_command,
    },
});

pub use self::zed::extension::*;

mod settings {
    #![allow(dead_code)]
    include!(concat!(env!("OUT_DIR"), "/since_v0.9.0/settings.rs"));
}

pub type ExtensionWorktree = Arc<dyn WorktreeDelegate>;
pub type ExtensionProject = Arc<dyn ProjectDelegate>;
pub type ExtensionKeyValueStore = Arc<dyn KeyValueStoreDelegate>;

pub fn linker(executor: &BackgroundExecutor) -> &'static Linker<WasmState> {
    static LINKER: OnceLock<Linker<WasmState>> = OnceLock::new();
    LINKER.get_or_init(|| {
        super::new_linker(executor, |linker| {
            Extension::add_to_linker::<_, WasmState>(linker, |s| s)
        })
    })
}

impl From<CodeLabel> for latest::CodeLabel {
    fn from(value: CodeLabel) -> Self {
        Self {
            code: value.code,
            spans: value.spans.into_iter().map(Into::into).collect(),
            filter_range: value.filter_range,
        }
    }
}

impl From<CodeLabelSpan> for latest::CodeLabelSpan {
    fn from(value: CodeLabelSpan) -> Self {
        match value {
            CodeLabelSpan::CodeRange(range) => Self::CodeRange(range),
            CodeLabelSpan::Literal(literal) => Self::Literal(literal.into()),
        }
    }
}

impl From<CodeLabelSpanLiteral> for latest::CodeLabelSpanLiteral {
    fn from(value: CodeLabelSpanLiteral) -> Self {
        Self {
            text: value.text,
            highlight_name: value.highlight_name,
        }
    }
}

impl From<SettingsLocation> for latest::SettingsLocation {
    fn from(value: SettingsLocation) -> Self {
        Self {
            worktree_id: value.worktree_id,
            path: value.path,
        }
    }
}

impl From<LanguageServerInstallationStatus> for latest::LanguageServerInstallationStatus {
    fn from(value: LanguageServerInstallationStatus) -> Self {
        match value {
            LanguageServerInstallationStatus::None => Self::None,
            LanguageServerInstallationStatus::Downloading => Self::Downloading,
            LanguageServerInstallationStatus::CheckingForUpdate => Self::CheckingForUpdate,
            LanguageServerInstallationStatus::Failed(message) => Self::Failed(message),
        }
    }
}

impl From<DownloadedFileType> for latest::DownloadedFileType {
    fn from(value: DownloadedFileType) -> Self {
        match value {
            DownloadedFileType::Gzip => Self::Gzip,
            DownloadedFileType::GzipTar => Self::GzipTar,
            DownloadedFileType::Zip => Self::Zip,
            DownloadedFileType::Uncompressed => Self::Uncompressed,
        }
    }
}

impl HostKeyValueStore for WasmState {
    async fn insert(
        &mut self,
        kv_store: Resource<ExtensionKeyValueStore>,
        key: String,
        value: String,
    ) -> wasmtime::Result<Result<(), String>> {
        latest::HostKeyValueStore::insert(self, kv_store, key, value).await
    }

    async fn drop(&mut self, _worktree: Resource<ExtensionKeyValueStore>) -> wasmtime::Result<()> {
        // We only ever hand out borrows of key-value stores.
        Ok(())
    }
}

impl HostProject for WasmState {
    async fn worktree_ids(
        &mut self,
        project: Resource<ExtensionProject>,
    ) -> wasmtime::Result<Vec<u64>> {
        latest::HostProject::worktree_ids(self, project).await
    }

    async fn drop(&mut self, _project: Resource<Project>) -> wasmtime::Result<()> {
        // We only ever hand out borrows of projects.
        Ok(())
    }
}

impl HostWorktree for WasmState {
    async fn id(&mut self, delegate: Resource<Arc<dyn WorktreeDelegate>>) -> wasmtime::Result<u64> {
        latest::HostWorktree::id(self, delegate).await
    }

    async fn root_path(
        &mut self,
        delegate: Resource<Arc<dyn WorktreeDelegate>>,
    ) -> wasmtime::Result<String> {
        latest::HostWorktree::root_path(self, delegate).await
    }

    async fn read_text_file(
        &mut self,
        delegate: Resource<Arc<dyn WorktreeDelegate>>,
        path: String,
    ) -> wasmtime::Result<Result<String, String>> {
        latest::HostWorktree::read_text_file(self, delegate, path).await
    }

    async fn shell_env(
        &mut self,
        delegate: Resource<Arc<dyn WorktreeDelegate>>,
    ) -> wasmtime::Result<EnvVars> {
        latest::HostWorktree::shell_env(self, delegate).await
    }

    async fn which(
        &mut self,
        delegate: Resource<Arc<dyn WorktreeDelegate>>,
        binary_name: String,
    ) -> wasmtime::Result<Option<String>> {
        latest::HostWorktree::which(self, delegate, binary_name).await
    }

    async fn drop(&mut self, _worktree: Resource<Worktree>) -> wasmtime::Result<()> {
        // We only ever hand out borrows of worktrees.
        Ok(())
    }
}

/// What an extension reads for the `visual_md` settings category: its own entry
/// under `visual_md.extensions`, or `null` when it has none, and when `key` is
/// given, that key of the entry. An extension is only ever handed its own entry.
fn extension_settings_json(
    settings: &::settings::VisualMdSettingsContent,
    extension_id: &str,
    key: Option<&str>,
) -> Result<String> {
    let entry = settings
        .extensions
        .as_ref()
        .and_then(|extensions| extensions.get(extension_id))
        .unwrap_or(&serde_json::Value::Null);
    let value = match key {
        Some(key) => entry.get(key).unwrap_or(&serde_json::Value::Null),
        None => entry,
    };
    Ok(serde_json::to_string(value)?)
}

impl ExtensionImports for WasmState {
    async fn get_settings(
        &mut self,
        location: Option<self::SettingsLocation>,
        category: String,
        key: Option<String>,
    ) -> wasmtime::Result<Result<String, String>> {
        if category == "visual_md" {
            let extension_id = self.manifest.id.clone();
            return self
                .on_main_thread(move |cx| {
                    async move {
                        let path = location.as_ref().and_then(|location| {
                            RelPath::new(Path::new(&location.path), PathStyle::Unix).ok()
                        });
                        let location =
                            path.as_ref()
                                .zip(location.as_ref())
                                .map(|(path, location)| ::settings::SettingsLocation {
                                    worktree_id: WorktreeId::from_proto(location.worktree_id),
                                    path,
                                });
                        cx.update(|cx| {
                            let markdown = LanguageName::new("Markdown");
                            let settings = AllLanguageSettings::get(location, cx).language(
                                location,
                                Some(&markdown),
                                cx,
                            );
                            extension_settings_json(
                                &settings.visual_md,
                                &extension_id,
                                key.as_deref(),
                            )
                        })
                    }
                    .boxed_local()
                })
                .await
                .to_wasmtime_result();
        }

        latest::ExtensionImports::get_settings(
            self,
            location.map(|location| location.into()),
            category,
            key,
        )
        .await
    }

    async fn set_language_server_installation_status(
        &mut self,
        server_name: String,
        status: LanguageServerInstallationStatus,
    ) -> wasmtime::Result<()> {
        latest::ExtensionImports::set_language_server_installation_status(
            self,
            server_name,
            status.into(),
        )
        .await
    }

    async fn download_file(
        &mut self,
        url: String,
        path: String,
        file_type: DownloadedFileType,
    ) -> wasmtime::Result<Result<(), String>> {
        latest::ExtensionImports::download_file(self, url, path, file_type.into()).await
    }

    async fn make_file_executable(&mut self, path: String) -> wasmtime::Result<Result<(), String>> {
        latest::ExtensionImports::make_file_executable(self, path).await
    }
}

impl visual_md::Host for WasmState {}

impl From<visual_md::SpanStyle> for extension::VisualMdSpanStyle {
    fn from(value: visual_md::SpanStyle) -> Self {
        Self {
            color: value.color,
            background_color: value.background_color,
            theme_token: value.theme_token,
            font_weight: value.font_weight,
            italic: value.italic,
            underline: value.underline,
            strikethrough: value.strikethrough,
        }
    }
}

impl From<visual_md::StyledSpan> for extension::VisualMdStyledSpan {
    fn from(value: visual_md::StyledSpan) -> Self {
        Self {
            range: value.range.into(),
            style: value.style.into(),
        }
    }
}

impl From<visual_md::StyledText> for extension::VisualMdStyledText {
    fn from(value: visual_md::StyledText) -> Self {
        Self {
            text: value.text,
            spans: value.spans.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<visual_md::ImageFormat> for extension::VisualMdImageFormat {
    fn from(value: visual_md::ImageFormat) -> Self {
        match value {
            visual_md::ImageFormat::Png => Self::Png,
            visual_md::ImageFormat::Jpeg => Self::Jpeg,
            visual_md::ImageFormat::Gif => Self::Gif,
            visual_md::ImageFormat::Webp => Self::Webp,
        }
    }
}

impl From<visual_md::FenceOutput> for extension::VisualMdFenceOutput {
    fn from(value: visual_md::FenceOutput) -> Self {
        match value {
            visual_md::FenceOutput::StyledText(text) => Self::StyledText(text.into()),
            visual_md::FenceOutput::Markdown(markdown) => Self::Markdown(markdown),
            visual_md::FenceOutput::Svg(svg) => Self::Svg(svg),
            visual_md::FenceOutput::Image(image) => Self::Image(extension::VisualMdImage {
                format: image.format.into(),
                bytes: image.bytes,
            }),
        }
    }
}

impl From<visual_md::FenceResult> for extension::VisualMdFenceResult {
    fn from(value: visual_md::FenceResult) -> Self {
        Self {
            output: value.output.into(),
            height_hint: value.height_hint,
        }
    }
}

impl From<visual_md::TextEdit> for extension::VisualMdTextEdit {
    fn from(value: visual_md::TextEdit) -> Self {
        Self {
            range: value.range.into(),
            new_text: value.new_text,
        }
    }
}

impl From<visual_md::CommandResult> for extension::VisualMdCommandResult {
    fn from(value: visual_md::CommandResult) -> Self {
        Self {
            edits: value.edits.into_iter().map(Into::into).collect(),
            selections: value
                .selections
                .map(|selections| selections.into_iter().map(Into::into).collect()),
            message: value.message,
        }
    }
}

impl From<extension::VisualMdAppearance> for visual_md::Appearance {
    fn from(value: extension::VisualMdAppearance) -> Self {
        match value {
            extension::VisualMdAppearance::Light => Self::Light,
            extension::VisualMdAppearance::Dark => Self::Dark,
        }
    }
}

impl From<extension::VisualMdFenceRequest> for visual_md::FenceRequest {
    fn from(value: extension::VisualMdFenceRequest) -> Self {
        Self {
            language: value.language,
            info: value.info,
            content: value.content,
            appearance: value.appearance.into(),
            path: value.path,
        }
    }
}

impl From<visual_md::Replacement> for extension::VisualMdReplacement {
    fn from(value: visual_md::Replacement) -> Self {
        Self {
            range: value.range.into(),
            text: value.text,
        }
    }
}

impl From<visual_md::RuleOutput> for extension::VisualMdRuleOutput {
    fn from(value: visual_md::RuleOutput) -> Self {
        Self {
            spans: value.spans.into_iter().map(Into::into).collect(),
            hidden: value.hidden.into_iter().map(Into::into).collect(),
            replacements: value.replacements.into_iter().map(Into::into).collect(),
        }
    }
}

impl TryFrom<extension::VisualMdRuleMatch> for visual_md::RuleMatch {
    type Error = anyhow::Error;

    fn try_from(value: extension::VisualMdRuleMatch) -> Result<Self> {
        Ok(Self {
            text: value.text,
            captures: value
                .captures
                .into_iter()
                .map(|capture| capture.map(range_to_wit).transpose())
                .collect::<Result<_>>()?,
        })
    }
}

impl From<visual_md::LinkTarget> for extension::VisualMdLinkTarget {
    fn from(value: visual_md::LinkTarget) -> Self {
        match value {
            visual_md::LinkTarget::Url(url) => Self::Url(url),
            visual_md::LinkTarget::File(path) => Self::File(path),
        }
    }
}

impl From<visual_md::CompletionItem> for extension::VisualMdCompletionItem {
    fn from(value: visual_md::CompletionItem) -> Self {
        Self {
            label: value.label,
            detail: value.detail,
            insert_text: value.insert_text,
        }
    }
}

impl From<extension::VisualMdLinkRequest> for visual_md::LinkRequest {
    fn from(value: extension::VisualMdLinkRequest) -> Self {
        Self {
            scheme: value.scheme,
            target: value.target,
            wikilink: value.wikilink,
            path: value.path,
        }
    }
}

impl From<extension::VisualMdCompletionRequest> for visual_md::CompletionRequest {
    fn from(value: extension::VisualMdCompletionRequest) -> Self {
        Self {
            query: value.query,
            path: value.path,
            files: value.files,
        }
    }
}

impl From<extension::VisualMdLinkStyle> for visual_md::LinkStyle {
    fn from(value: extension::VisualMdLinkStyle) -> Self {
        match value {
            extension::VisualMdLinkStyle::Inline => Self::Inline,
            extension::VisualMdLinkStyle::Wikilink => Self::Wikilink,
            extension::VisualMdLinkStyle::Embed => Self::Embed,
        }
    }
}

impl From<extension::VisualMdDocumentEventKind> for visual_md::DocumentEventKind {
    fn from(value: extension::VisualMdDocumentEventKind) -> Self {
        match value {
            extension::VisualMdDocumentEventKind::Opened => Self::Opened,
            extension::VisualMdDocumentEventKind::Saved => Self::Saved,
            extension::VisualMdDocumentEventKind::Changed => Self::Changed,
        }
    }
}

impl TryFrom<extension::VisualMdOutline> for visual_md::Outline {
    type Error = anyhow::Error;

    fn try_from(value: extension::VisualMdOutline) -> Result<Self> {
        Ok(Self {
            headings: value
                .headings
                .into_iter()
                .map(|heading| {
                    Ok(visual_md::OutlineHeading {
                        level: heading.level,
                        text: heading.text,
                        range: range_to_wit(heading.range)?,
                    })
                })
                .collect::<Result<_>>()?,
            links: value
                .links
                .into_iter()
                .map(|link| {
                    Ok(visual_md::OutlineLink {
                        style: link.style.into(),
                        target: link.target,
                        text: link.text,
                        range: range_to_wit(link.range)?,
                    })
                })
                .collect::<Result<_>>()?,
            tags: value
                .tags
                .into_iter()
                .map(|tag| {
                    Ok(visual_md::OutlineTag {
                        name: tag.name,
                        range: range_to_wit(tag.range)?,
                    })
                })
                .collect::<Result<_>>()?,
            tasks: value
                .tasks
                .into_iter()
                .map(|task| {
                    Ok(visual_md::OutlineTask {
                        text: task.text,
                        checked: task.checked,
                        range: range_to_wit(task.range)?,
                    })
                })
                .collect::<Result<_>>()?,
            frontmatter: value.frontmatter,
        })
    }
}

impl TryFrom<extension::VisualMdDocumentEvent> for visual_md::DocumentEvent {
    type Error = anyhow::Error;

    fn try_from(value: extension::VisualMdDocumentEvent) -> Result<Self> {
        Ok(Self {
            kind: value.kind.into(),
            path: value.path,
            outline: value.outline.try_into()?,
        })
    }
}

fn range_to_wit(range: std::ops::Range<usize>) -> Result<Range> {
    Ok(Range {
        start: u32::try_from(range.start)?,
        end: u32::try_from(range.end)?,
    })
}

impl TryFrom<extension::VisualMdCommandContext> for visual_md::CommandContext {
    type Error = anyhow::Error;

    fn try_from(value: extension::VisualMdCommandContext) -> Result<Self> {
        Ok(Self {
            text: value.text,
            selections: value
                .selections
                .into_iter()
                .map(range_to_wit)
                .collect::<Result<_>>()?,
            path: value.path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_range_to_wit_rejects_offsets_past_u32() {
        assert_eq!(
            range_to_wit(3..9)
                .map(|range| (range.start, range.end))
                .ok(),
            Some((3, 9))
        );
        assert!(range_to_wit(0..u32::MAX as usize + 1).is_err());
    }

    #[test]
    fn test_command_context_conversion_fails_for_out_of_range_selections() {
        let context = |selection: std::ops::Range<usize>| extension::VisualMdCommandContext {
            text: String::new(),
            selections: vec![selection],
            path: None,
        };

        assert!(visual_md::CommandContext::try_from(context(0..4)).is_ok());
        assert!(visual_md::CommandContext::try_from(context(0..u32::MAX as usize + 1)).is_err());
    }

    #[test]
    fn test_rule_matches_convert_with_their_optional_captures() {
        let converted = visual_md::RuleMatch::try_from(extension::VisualMdRuleMatch {
            text: ":smile:".to_string(),
            captures: vec![Some(0..7), None, Some(1..6)],
        })
        .expect("the ranges fit");

        assert_eq!(converted.text, ":smile:");
        assert_eq!(
            converted
                .captures
                .iter()
                .map(|capture| capture.as_ref().map(|range| (range.start, range.end)))
                .collect::<Vec<_>>(),
            vec![Some((0, 7)), None, Some((1, 6))]
        );
        assert!(
            visual_md::RuleMatch::try_from(extension::VisualMdRuleMatch {
                text: String::new(),
                captures: vec![Some(0..u32::MAX as usize + 1)],
            })
            .is_err()
        );
    }

    #[test]
    fn test_rule_outputs_convert_spans_hidden_ranges_and_replacements() {
        let output = extension::VisualMdRuleOutput::from(visual_md::RuleOutput {
            spans: vec![visual_md::StyledSpan {
                range: Range { start: 1, end: 3 },
                style: visual_md::SpanStyle {
                    color: Some("#fff".into()),
                    background_color: None,
                    theme_token: None,
                    font_weight: None,
                    italic: Some(true),
                    underline: None,
                    strikethrough: None,
                },
            }],
            hidden: vec![Range { start: 0, end: 1 }],
            replacements: vec![visual_md::Replacement {
                range: Range { start: 1, end: 6 },
                text: "😀".into(),
            }],
        });

        assert_eq!(output.spans.len(), 1);
        assert_eq!(output.spans[0].range, 1..3);
        assert_eq!(output.hidden, vec![0..1]);
        assert_eq!(
            output.replacements,
            vec![extension::VisualMdReplacement {
                range: 1..6,
                text: "😀".to_string(),
            }]
        );
    }

    fn outline() -> extension::VisualMdOutline {
        extension::VisualMdOutline {
            headings: vec![extension::VisualMdOutlineHeading {
                level: 2,
                text: "Title".to_string(),
                range: 0..8,
            }],
            links: vec![
                extension::VisualMdOutlineLink {
                    style: extension::VisualMdLinkStyle::Inline,
                    target: "https://x.org".to_string(),
                    text: Some("x".to_string()),
                    range: 10..30,
                },
                extension::VisualMdOutlineLink {
                    style: extension::VisualMdLinkStyle::Embed,
                    target: "pic".to_string(),
                    text: None,
                    range: 31..38,
                },
            ],
            tags: vec![extension::VisualMdOutlineTag {
                name: "idea".to_string(),
                range: 40..45,
            }],
            tasks: vec![extension::VisualMdOutlineTask {
                text: "do it".to_string(),
                checked: true,
                range: 50..53,
            }],
            frontmatter: Some("title: x".to_string()),
        }
    }

    #[test]
    fn test_a_document_event_converts_with_its_whole_outline() {
        let event = visual_md::DocumentEvent::try_from(extension::VisualMdDocumentEvent {
            kind: extension::VisualMdDocumentEventKind::Saved,
            path: Some("/notes/a.md".to_string()),
            outline: outline(),
        })
        .expect("the ranges fit");

        assert!(matches!(event.kind, visual_md::DocumentEventKind::Saved));
        assert_eq!(event.path.as_deref(), Some("/notes/a.md"));
        assert_eq!(event.outline.headings.len(), 1);
        assert_eq!(event.outline.headings[0].level, 2);
        assert_eq!(
            (
                event.outline.headings[0].range.start,
                event.outline.headings[0].range.end
            ),
            (0, 8)
        );
        assert!(matches!(
            event.outline.links[0].style,
            visual_md::LinkStyle::Inline
        ));
        assert!(matches!(
            event.outline.links[1].style,
            visual_md::LinkStyle::Embed
        ));
        assert_eq!(event.outline.links[1].text, None);
        assert_eq!(event.outline.tags[0].name, "idea");
        assert!(event.outline.tasks[0].checked);
        assert_eq!(event.outline.frontmatter.as_deref(), Some("title: x"));
    }

    #[test]
    fn test_a_document_event_with_a_range_past_u32_fails_to_convert() {
        let mut huge = outline();
        huge.tags[0].range = 0..u32::MAX as usize + 1;

        assert!(
            visual_md::DocumentEvent::try_from(extension::VisualMdDocumentEvent {
                kind: extension::VisualMdDocumentEventKind::Changed,
                path: None,
                outline: huge,
            })
            .is_err()
        );
    }

    #[test]
    fn test_links_and_completions_convert_both_ways() {
        let request = visual_md::LinkRequest::from(extension::VisualMdLinkRequest {
            scheme: Some("notes".to_string()),
            target: "notes://a".to_string(),
            wikilink: false,
            path: None,
        });
        assert_eq!(request.scheme.as_deref(), Some("notes"));
        assert_eq!(request.target, "notes://a");
        assert!(!request.wikilink);

        assert_eq!(
            extension::VisualMdLinkTarget::from(visual_md::LinkTarget::Url("https://x.org".into())),
            extension::VisualMdLinkTarget::Url("https://x.org".to_string())
        );
        assert_eq!(
            extension::VisualMdLinkTarget::from(visual_md::LinkTarget::File("a.md".into())),
            extension::VisualMdLinkTarget::File("a.md".to_string())
        );

        let completion = visual_md::CompletionRequest::from(extension::VisualMdCompletionRequest {
            query: "ab".to_string(),
            path: Some("/n/a.md".to_string()),
            files: vec!["a.md".to_string(), "b/c.md".to_string()],
        });
        assert_eq!(completion.query, "ab");
        assert_eq!(completion.files, vec!["a.md", "b/c.md"]);

        assert_eq!(
            extension::VisualMdCompletionItem::from(visual_md::CompletionItem {
                label: "Alpha".into(),
                detail: Some("a.md".into()),
                insert_text: "alpha".into(),
            }),
            extension::VisualMdCompletionItem {
                label: "Alpha".to_string(),
                detail: Some("a.md".to_string()),
                insert_text: "alpha".to_string(),
            }
        );
    }

    fn settings_with_extensions(
        extensions: serde_json::Value,
    ) -> ::settings::VisualMdSettingsContent {
        ::settings::VisualMdSettingsContent {
            extensions: serde_json::from_value(extensions).ok(),
            ..Default::default()
        }
    }

    #[test]
    fn test_an_extension_is_given_only_its_own_settings() {
        let settings = settings_with_extensions(serde_json::json!({
            "mine": { "notes": ["a", "b"], "limit": 3 },
            "theirs": { "secret": "token" },
        }));

        let json = |id: &str, key: Option<&str>| {
            extension_settings_json(&settings, id, key).expect("the settings serialize")
        };

        assert_eq!(json("mine", None), r#"{"notes":["a","b"],"limit":3}"#);
        assert_eq!(json("theirs", None), r#"{"secret":"token"}"#);
        assert_eq!(json("mine", Some("notes")), r#"["a","b"]"#);
        assert!(!json("mine", None).contains("token"));
        assert!(!json("mine", Some("secret")).contains("token"));
    }

    #[test]
    fn test_missing_settings_are_null() {
        let settings = settings_with_extensions(serde_json::json!({ "mine": { "a": 1 } }));

        assert_eq!(
            extension_settings_json(&settings, "other", None)
                .ok()
                .as_deref(),
            Some("null")
        );
        assert_eq!(
            extension_settings_json(&settings, "mine", Some("missing"))
                .ok()
                .as_deref(),
            Some("null")
        );
        assert_eq!(
            extension_settings_json(&Default::default(), "mine", None)
                .ok()
                .as_deref(),
            Some("null")
        );
    }

    #[test]
    fn test_command_result_converts_edits_and_selections() {
        let result = extension::VisualMdCommandResult::from(visual_md::CommandResult {
            edits: vec![visual_md::TextEdit {
                range: Range { start: 1, end: 3 },
                new_text: "x".into(),
            }],
            selections: Some(vec![Range { start: 2, end: 2 }]),
            message: Some("done".into()),
        });

        assert_eq!(
            result,
            extension::VisualMdCommandResult {
                edits: vec![extension::VisualMdTextEdit {
                    range: 1..3,
                    new_text: "x".into(),
                }],
                selections: Some(vec![2..2]),
                message: Some("done".into()),
            }
        );
    }

    #[test]
    fn test_fence_result_converts_every_output_kind() {
        let convert = |output| {
            extension::VisualMdFenceResult::from(visual_md::FenceResult {
                output,
                height_hint: Some(4),
            })
        };

        assert_eq!(
            convert(visual_md::FenceOutput::Markdown("# hi".into())).output,
            extension::VisualMdFenceOutput::Markdown("# hi".into())
        );
        assert_eq!(
            convert(visual_md::FenceOutput::Svg("<svg/>".into())).output,
            extension::VisualMdFenceOutput::Svg("<svg/>".into())
        );
        assert_eq!(
            convert(visual_md::FenceOutput::Image(visual_md::Image {
                format: visual_md::ImageFormat::Webp,
                bytes: vec![1, 2, 3],
            }))
            .output,
            extension::VisualMdFenceOutput::Image(extension::VisualMdImage {
                format: extension::VisualMdImageFormat::Webp,
                bytes: vec![1, 2, 3],
            })
        );
        assert_eq!(
            convert(visual_md::FenceOutput::StyledText(visual_md::StyledText {
                text: "ab".into(),
                spans: vec![visual_md::StyledSpan {
                    range: Range { start: 0, end: 1 },
                    style: visual_md::SpanStyle {
                        color: Some("#fff".into()),
                        background_color: None,
                        theme_token: None,
                        font_weight: Some(700),
                        italic: None,
                        underline: Some(true),
                        strikethrough: None,
                    },
                }],
            }))
            .output,
            extension::VisualMdFenceOutput::StyledText(extension::VisualMdStyledText {
                text: "ab".into(),
                spans: vec![extension::VisualMdStyledSpan {
                    range: 0..1,
                    style: extension::VisualMdSpanStyle {
                        color: Some("#fff".into()),
                        font_weight: Some(700),
                        underline: Some(true),
                        ..Default::default()
                    },
                }],
            })
        );
        assert_eq!(
            convert(visual_md::FenceOutput::Svg(String::new())).height_hint,
            Some(4)
        );
    }
}

use std::{
    future::Future,
    ops::Range,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow};
use edit_prediction_types::{
    EditPrediction, EditPredictionDelegate, EditPredictionDiscardReason, EditPredictionIconSet,
    EditPredictionRequestTrigger, PredictedCursorPosition, SuggestionDisplayType,
    interpolate_edits,
};
use gpui::{App, AppContext as _, Context, Entity, EntityId, SharedString, Task, WeakEntity};
use icons::IconName;
use language::{Anchor, Buffer, BufferSnapshot, EditPreview, language_settings::LanguageSettings};
use project::Project;
use text::ToOffset as _;
use util::ResultExt as _;

use crate::{
    Devin,
    language_server::{CompletionFeedback, DevinLanguageServer},
    protocol::{
        Document, EditorOptions, HandleStreamingTabV2Response, ProviderSource, SupercompleteDiff,
        SupercompleteTriggerCondition, apply_edits, convert_diff, diff_targets_document,
        jump_offset, locate_new_offset,
    },
};

/// Devin's extension sends open documents as context up to this total size.
const MAX_OTHER_DOCUMENTS_BYTES: usize = 1024 * 1024;

pub struct DevinEditPredictionDelegate {
    devin: Entity<Devin>,
    project: Option<WeakEntity<Project>>,
    pending_request: Option<Task<Result<()>>>,
    current_prediction: Option<CurrentPrediction>,
    /// A suggestion in another file, shown as a jump to that file.
    jump_to_other_file: Option<JumpToOtherFile>,
}

pub(crate) struct CurrentPrediction {
    buffer_id: EntityId,
    server: Arc<DevinLanguageServer>,
    completion: ServerCompletion,
    /// The jump suggestion this prediction's edit was requested for, if any.
    tab_jump: Option<ServerCompletion>,
    document: Document,
    /// The document text with the prediction applied.
    completion_text: String,
    snapshot: BufferSnapshot,
    edits: Arc<[(Range<Anchor>, Arc<str>)]>,
    cursor_position: Option<PredictedCursorPosition>,
    edit_preview: EditPreview,
    shown_at: Option<Instant>,
}

struct JumpToOtherFile {
    source_buffer_id: EntityId,
    completion_id: SharedString,
    snapshot: BufferSnapshot,
    target: Anchor,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ServerCompletion {
    completion_id: SharedString,
    prompt_id: String,
}

impl ServerCompletion {
    fn new(response: &HandleStreamingTabV2Response) -> Self {
        Self {
            completion_id: response.completion_id.clone().into(),
            prompt_id: response.prompt_id.clone(),
        }
    }
}

/// The document a suggestion applies to: the one the request was made for, or one of the other
/// open documents sent as context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TargetDocument {
    Active,
    Other(usize),
}

#[derive(Debug)]
enum Suggestion {
    Edit {
        target: TargetDocument,
        completion: ServerCompletion,
        /// The jump suggestion the edit was requested for, if any.
        tab_jump: Option<ServerCompletion>,
        diff: SupercompleteDiff,
    },
    JumpToOtherDocument {
        document_index: usize,
        offset: usize,
        completion: ServerCompletion,
    },
}

enum ResolvedPrediction {
    Local(CurrentPrediction),
    OtherFile(CurrentPrediction),
    Jump(JumpToOtherFile),
}

/// Requests completions, abstracted so that suggestion handling can be tested without a server.
trait CompletionClient {
    fn complete(
        &self,
        document: Document,
        other_documents: Vec<Document>,
        editor_options: EditorOptions,
        trigger: SupercompleteTriggerCondition,
    ) -> impl Future<Output = Result<HandleStreamingTabV2Response>> + Send;
}

impl CompletionClient for DevinLanguageServer {
    fn complete(
        &self,
        document: Document,
        other_documents: Vec<Document>,
        editor_options: EditorOptions,
        trigger: SupercompleteTriggerCondition,
    ) -> impl Future<Output = Result<HandleStreamingTabV2Response>> + Send {
        DevinLanguageServer::complete(self, document, other_documents, editor_options, trigger)
    }
}

impl CurrentPrediction {
    fn interpolate(&self, new_snapshot: &BufferSnapshot) -> Option<Vec<(Range<Anchor>, Arc<str>)>> {
        interpolate_edits(&self.snapshot, new_snapshot, &self.edits)
            .filter(|edits| !edits.is_empty())
    }

    pub(crate) fn buffer_id(&self) -> EntityId {
        self.buffer_id
    }

    /// Tells Devin whether the prediction was accepted. Predictions that were never shown aren't
    /// reported, matching Devin's own extension.
    fn report_outcome(self, is_accepted: bool, is_intentional_reject: bool, cx: &mut App) {
        let Some(shown_at) = self.shown_at else {
            return;
        };
        let feedback_delay = shown_at.elapsed();
        let Self {
            server,
            completion,
            tab_jump,
            document,
            completion_text,
            ..
        } = self;
        let mut feedback = Vec::new();
        if let Some(tab_jump) = tab_jump {
            feedback.push(CompletionFeedback {
                source: ProviderSource::TabJump,
                completion_id: tab_jump.completion_id.to_string(),
                prompt_id: tab_jump.prompt_id,
                document: document.clone(),
                completion_text: String::new(),
                is_accepted,
                is_intentional_reject,
                feedback_delay,
            });
        }
        feedback.push(CompletionFeedback {
            source: ProviderSource::Supercomplete,
            completion_id: completion.completion_id.to_string(),
            prompt_id: completion.prompt_id,
            document,
            completion_text,
            is_accepted,
            is_intentional_reject,
            feedback_delay,
        });
        cx.background_spawn(async move {
            for feedback in feedback {
                server.provide_feedback(feedback).await.log_err();
            }
        })
        .detach();
    }
}

impl DevinEditPredictionDelegate {
    pub fn new(devin: Entity<Devin>, project: Option<WeakEntity<Project>>) -> Self {
        Self {
            devin,
            project,
            pending_request: None,
            current_prediction: None,
            jump_to_other_file: None,
        }
    }

    fn set_prediction(&mut self, prediction: Option<CurrentPrediction>, cx: &mut App) {
        if let Some(previous) = std::mem::replace(&mut self.current_prediction, prediction) {
            previous.report_outcome(false, false, cx);
        }
    }

    /// Takes over a prediction that another editor's request produced for this buffer, so that
    /// following a jump to this file shows the edit the jump was for.
    fn adopt_prediction_for_buffer(
        &mut self,
        buffer: &Entity<Buffer>,
        cx: &mut Context<Self>,
    ) -> bool {
        let buffer_id = buffer.entity_id();
        let Some(prediction) = self
            .devin
            .update(cx, |devin, _| devin.take_prediction_for_buffer(buffer_id))
        else {
            return false;
        };
        if prediction.interpolate(&buffer.read(cx).snapshot()).is_none() {
            return false;
        }
        self.jump_to_other_file = None;
        self.set_prediction(Some(prediction), cx);
        true
    }

    /// Collects the project's other open files, most recently used first, within the size budget.
    fn other_document_sources(
        &self,
        buffer: &Entity<Buffer>,
        cx: &App,
    ) -> Vec<(Entity<Buffer>, DocumentSource)> {
        let Some(project) = self.project.as_ref().and_then(|project| project.upgrade()) else {
            return Vec::new();
        };
        let recent_buffers = self.devin.read(cx).recent_buffers();
        let mut buffers = project.read(cx).opened_buffers(cx);
        buffers.retain(|other| other.entity_id() != buffer.entity_id());
        buffers.sort_by_key(|other| {
            recent_buffers
                .iter()
                .position(|recent| *recent == other.entity_id())
                .unwrap_or(usize::MAX)
        });

        let mut total_len = 0;
        buffers
            .into_iter()
            .filter_map(|other| {
                let source = DocumentSource::new(other.read(cx), cx)?;
                let len = source.snapshot.len();
                (total_len + len < MAX_OTHER_DOCUMENTS_BYTES).then(|| {
                    total_len += len;
                    (other, source)
                })
            })
            .collect()
    }
}

impl EditPredictionDelegate for DevinEditPredictionDelegate {
    fn name() -> &'static str {
        "devin"
    }

    fn display_name() -> &'static str {
        "Devin"
    }

    fn show_predictions_in_menu() -> bool {
        true
    }

    fn icons(&self, _cx: &App) -> EditPredictionIconSet {
        EditPredictionIconSet::new(IconName::AiDevin)
    }

    fn is_enabled(&self, buffer: &Entity<Buffer>, _cursor_position: Anchor, cx: &App) -> bool {
        self.devin.read(cx).is_signed_in()
            && buffer
                .read(cx)
                .file()
                .is_some_and(|file| file.as_local().is_some() && !file.is_private())
    }

    fn is_refreshing(&self, _cx: &App) -> bool {
        self.pending_request.is_some()
    }

    fn refresh(
        &mut self,
        buffer: Entity<Buffer>,
        cursor_position: Anchor,
        debounce_duration: Duration,
        trigger: EditPredictionRequestTrigger,
        cx: &mut Context<Self>,
    ) {
        let snapshot = buffer.read(cx).snapshot();
        if let Some(current_prediction) = &self.current_prediction
            && current_prediction.buffer_id == buffer.entity_id()
            && current_prediction.interpolate(&snapshot).is_some()
        {
            return;
        }
        if self.adopt_prediction_for_buffer(&buffer, cx) {
            cx.notify();
            return;
        }

        let Some(source) = DocumentSource::new(buffer.read(cx), cx) else {
            return;
        };
        let buffer_id = buffer.entity_id();
        let Some(server) = self.devin.update(cx, |devin, cx| {
            devin.record_buffer_access(buffer_id);
            devin.server(&source.workspace_uri, cx)
        }) else {
            return;
        };
        let (other_buffers, other_sources): (Vec<_>, Vec<_>) = self
            .other_document_sources(&buffer, cx)
            .into_iter()
            .map(|(other_buffer, source)| ((other_buffer, source.snapshot.clone()), source))
            .unzip();
        let editor_options = {
            let settings = LanguageSettings::for_buffer(buffer.read(cx), cx);
            EditorOptions {
                tab_size: u64::from(settings.tab_size.get()),
                insert_spaces: !settings.hard_tabs,
            }
        };
        let trigger_condition = match trigger {
            EditPredictionRequestTrigger::BufferEdit => SupercompleteTriggerCondition::Typing,
            EditPredictionRequestTrigger::Explicit => SupercompleteTriggerCondition::Forced,
            EditPredictionRequestTrigger::PredictionAccepted => {
                SupercompleteTriggerCondition::SupercompleteAccept
            }
            _ => SupercompleteTriggerCondition::CursorLineNavigation,
        };
        let cursor_offset = cursor_position.to_offset(&snapshot);

        self.pending_request = Some(cx.spawn(async move |this, cx| {
            if !debounce_duration.is_zero() {
                cx.background_executor().timer(debounce_duration).await;
            }

            let result = async {
                let server = server.await.map_err(|error| anyhow!("{error}"))?;
                let (document, other_documents, suggestion) = cx
                    .background_spawn({
                        let server = server.clone();
                        async move {
                            let document = source.into_document().with_cursor(cursor_offset);
                            let other_documents = other_sources
                                .into_iter()
                                .map(DocumentSource::into_document)
                                .collect::<Vec<_>>();
                            let suggestion = request_suggestion(
                                server.as_ref(),
                                &document,
                                &other_documents,
                                editor_options,
                                trigger_condition,
                            )
                            .await?;
                            anyhow::Ok((document, other_documents, suggestion))
                        }
                    })
                    .await?;

                let (target, completion, tab_jump, diff) = match suggestion {
                    None => return Ok(None),
                    Some(Suggestion::JumpToOtherDocument {
                        document_index,
                        offset,
                        completion,
                    }) => {
                        let (_, target_snapshot) = other_buffers
                            .get(document_index)
                            .context("jump target is not an open document")?;
                        return Ok(Some(ResolvedPrediction::Jump(JumpToOtherFile {
                            source_buffer_id: buffer_id,
                            completion_id: completion.completion_id,
                            snapshot: target_snapshot.clone(),
                            target: target_snapshot.anchor_before(offset),
                        })));
                    }
                    Some(Suggestion::Edit {
                        target,
                        completion,
                        tab_jump,
                        diff,
                    }) => (target, completion, tab_jump, diff),
                };

                let (target_buffer, target_snapshot, target_document) = match target {
                    TargetDocument::Active => (buffer.clone(), snapshot, document),
                    TargetDocument::Other(index) => {
                        let (target_buffer, target_snapshot) = other_buffers
                            .get(index)
                            .context("edit target is not an open document")?;
                        let target_document = other_documents
                            .into_iter()
                            .nth(index)
                            .context("edit target is not an open document")?;
                        (
                            target_buffer.clone(),
                            target_snapshot.clone(),
                            target_document,
                        )
                    }
                };
                let (target_document, converted, completion_text) = cx
                    .background_spawn(async move {
                        let converted = convert_diff(&target_document.text, &diff)?;
                        let completion_text =
                            apply_edits(&target_document.text, &converted.edits);
                        anyhow::Ok((target_document, converted, completion_text))
                    })
                    .await
                    .context("Devin returned a diff that doesn't match the buffer")?;
                if converted.edits.is_empty() {
                    return Ok(None);
                }

                let edits: Arc<[(Range<Anchor>, Arc<str>)]> = converted
                    .edits
                    .iter()
                    .map(|(range, new_text)| {
                        (
                            target_snapshot.anchor_after(range.start)
                                ..target_snapshot.anchor_before(range.end),
                            Arc::from(new_text.as_str()),
                        )
                    })
                    .collect();
                let cursor_position = converted.cursor_offset.map(|new_offset| {
                    let (old_offset, offset_in_edit) =
                        locate_new_offset(&converted.edits, new_offset);
                    let edit_anchor = edits
                        .iter()
                        .zip(&converted.edits)
                        .find(|(_, (range, _))| range.start == old_offset && offset_in_edit > 0)
                        .map(|((range, _), _)| range.start);
                    match edit_anchor {
                        Some(anchor) => PredictedCursorPosition::new(anchor, offset_in_edit),
                        None => PredictedCursorPosition::at_anchor(
                            target_snapshot.anchor_before(old_offset),
                        ),
                    }
                });
                let edit_preview = target_buffer
                    .read_with(cx, |buffer, cx| buffer.preview_edits(edits.clone(), cx))
                    .await;

                let prediction = CurrentPrediction {
                    buffer_id: target_buffer.entity_id(),
                    server,
                    completion,
                    tab_jump,
                    document: target_document,
                    completion_text,
                    snapshot: target_snapshot,
                    edits,
                    cursor_position,
                    edit_preview,
                    shown_at: None,
                };
                anyhow::Ok(Some(match target {
                    TargetDocument::Active => ResolvedPrediction::Local(prediction),
                    TargetDocument::Other(_) => ResolvedPrediction::OtherFile(prediction),
                }))
            }
            .await;

            this.update(cx, |this, cx| {
                this.pending_request = None;
                let resolved = match result {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        log::error!("Devin edit prediction failed: {error:#}");
                        None
                    }
                };
                this.jump_to_other_file = None;
                match resolved {
                    None => this.set_prediction(None, cx),
                    Some(ResolvedPrediction::Local(prediction)) => {
                        this.set_prediction(Some(prediction), cx)
                    }
                    Some(ResolvedPrediction::OtherFile(prediction)) => {
                        this.set_prediction(None, cx);
                        if let Some((target, _)) = prediction.edits.first() {
                            this.jump_to_other_file = Some(JumpToOtherFile {
                                source_buffer_id: buffer_id,
                                completion_id: prediction.completion.completion_id.clone(),
                                snapshot: prediction.snapshot.clone(),
                                target: target.start,
                            });
                        }
                        this.devin.update(cx, |devin, _| {
                            devin.set_prediction_for_other_buffer(prediction)
                        });
                    }
                    Some(ResolvedPrediction::Jump(jump)) => {
                        this.set_prediction(None, cx);
                        this.jump_to_other_file = Some(jump);
                    }
                }
                cx.notify();
            })
        }));
    }

    fn accept(&mut self, cx: &mut Context<Self>) {
        self.pending_request = None;
        self.jump_to_other_file = None;
        if let Some(prediction) = self.current_prediction.take() {
            prediction.report_outcome(true, false, cx);
        }
    }

    fn discard(&mut self, reason: EditPredictionDiscardReason, cx: &mut Context<Self>) {
        self.pending_request = None;
        self.jump_to_other_file = None;
        if let Some(prediction) = self.current_prediction.take() {
            prediction.report_outcome(
                false,
                reason == EditPredictionDiscardReason::Rejected,
                cx,
            );
        }
    }

    fn did_show(&mut self, _display_type: SuggestionDisplayType, _cx: &mut Context<Self>) {
        if let Some(prediction) = &mut self.current_prediction {
            prediction.shown_at.get_or_insert_with(Instant::now);
        }
    }

    fn suggest(
        &mut self,
        buffer: &Entity<Buffer>,
        _cursor_position: Anchor,
        cx: &mut Context<Self>,
    ) -> Option<EditPrediction> {
        if let Some(jump) = &self.jump_to_other_file
            && jump.source_buffer_id == buffer.entity_id()
        {
            return Some(EditPrediction::Jump {
                id: Some(jump.completion_id.clone()),
                snapshot: jump.snapshot.clone(),
                target: jump.target,
            });
        }

        let has_prediction_for_buffer = self
            .current_prediction
            .as_ref()
            .is_some_and(|prediction| prediction.buffer_id == buffer.entity_id());
        if !has_prediction_for_buffer && !self.adopt_prediction_for_buffer(buffer, cx) {
            return None;
        }
        let current_prediction = self.current_prediction.as_ref()?;
        let snapshot = buffer.read(cx).snapshot();
        let edits = current_prediction.interpolate(&snapshot)?;
        // The predicted cursor is only meaningful for the edits exactly as they were predicted.
        let unchanged = snapshot.version == current_prediction.snapshot.version;
        Some(EditPrediction::Local {
            id: Some(current_prediction.completion.completion_id.clone()),
            edits,
            cursor_position: current_prediction.cursor_position.filter(|_| unchanged),
            edit_preview: Some(current_prediction.edit_preview.clone()),
        })
    }
}

/// Finds the document a suggestion's path refers to, preferring the active document.
fn find_target_document(
    path: &str,
    document: &Document,
    other_documents: &[Document],
) -> Option<TargetDocument> {
    if diff_targets_document(path, &document.absolute_uri) {
        return Some(TargetDocument::Active);
    }
    other_documents
        .iter()
        .position(|other| !path.is_empty() && diff_targets_document(path, &other.absolute_uri))
        .map(TargetDocument::Other)
}

/// Requests a suggestion at the cursor. When Devin suggests jumping elsewhere in the active
/// document, this requests the edit at the jump target instead, because Zed shows edits away from
/// the cursor behind its own jump affordance, while a bare jump within a file has no equivalent.
/// Jumps to other open documents are returned as they are.
async fn request_suggestion(
    client: &impl CompletionClient,
    document: &Document,
    other_documents: &[Document],
    editor_options: EditorOptions,
    trigger: SupercompleteTriggerCondition,
) -> Result<Option<Suggestion>> {
    let mut response = client
        .complete(
            document.clone(),
            other_documents.to_vec(),
            editor_options.clone(),
            trigger,
        )
        .await?;

    let mut tab_jump = None;
    if response.diff.is_none()
        && let Some(jump) = response.tab_jump.take()
    {
        let Some(position) = jump.jump_position else {
            log::debug!("Devin suggested a jump without a position");
            return Ok(None);
        };
        match find_target_document(&jump.path, document, other_documents) {
            Some(TargetDocument::Active) => {
                let Some(offset) = jump_offset(&document.text, position) else {
                    log::debug!("Devin suggested a jump outside the document");
                    return Ok(None);
                };
                tab_jump = Some(ServerCompletion::new(&response));
                response = client
                    .complete(
                        document.with_cursor(offset),
                        other_documents.to_vec(),
                        editor_options,
                        SupercompleteTriggerCondition::CursorLineNavigation,
                    )
                    .await
                    .context("failed to request the edit at Devin's jump target")?;
            }
            Some(TargetDocument::Other(document_index)) => {
                let Some(offset) = other_documents
                    .get(document_index)
                    .and_then(|other| jump_offset(&other.text, position))
                else {
                    log::debug!("Devin suggested a jump outside the target document");
                    return Ok(None);
                };
                return Ok(Some(Suggestion::JumpToOtherDocument {
                    document_index,
                    offset,
                    completion: ServerCompletion::new(&response),
                }));
            }
            None => {
                log::debug!("Devin suggested a jump to a file that isn't open");
                return Ok(None);
            }
        }
    }

    let completion = ServerCompletion::new(&response);
    let Some(diff) = response.diff else {
        log::debug!(
            "Devin returned no edit (stop reason: {:?}, filter reason: {:?})",
            response.stop_reason,
            response.filter_reason
        );
        return Ok(None);
    };
    let Some(target) = find_target_document(&diff.path, document, other_documents) else {
        log::debug!("Devin suggested an edit in a file that isn't open");
        return Ok(None);
    };
    Ok(Some(Suggestion::Edit {
        target,
        completion,
        tab_jump,
        diff,
    }))
}

/// A buffer captured on the main thread, so that its text can be built in the background.
struct DocumentSource {
    snapshot: BufferSnapshot,
    absolute_uri: String,
    workspace_uri: String,
    is_dirty: bool,
}

impl DocumentSource {
    /// Returns `None` for buffers that aren't local files, and for private files, which aren't
    /// sent to Devin.
    fn new(buffer: &Buffer, cx: &App) -> Option<Self> {
        let file = buffer.file()?;
        if file.is_private() {
            return None;
        }
        let abs_path = file.as_local()?.abs_path(cx);
        let worktree_root = worktree_root(&abs_path, file.path().components().count());
        Some(Self {
            snapshot: buffer.snapshot(),
            absolute_uri: url::Url::from_file_path(&abs_path).ok()?.to_string(),
            workspace_uri: url::Url::from_directory_path(&worktree_root)
                .ok()?
                .to_string(),
            is_dirty: buffer.is_dirty(),
        })
    }

    fn into_document(self) -> Document {
        let language_id = self
            .snapshot
            .language()
            .map(|language| language.lsp_id())
            .unwrap_or_default();
        Document {
            absolute_uri: self.absolute_uri,
            workspace_uri: self.workspace_uri,
            // Zed buffers store LF line endings internally, so send the text and diff it in that
            // form, rather than the line ending used on disk.
            text: self.snapshot.text(),
            editor_language: if language_id.is_empty() {
                "plaintext".to_string()
            } else {
                editor_language_id(&language_id)
            },
            language: Some(language_enum(&language_id)),
            cursor_offset: 0,
            cursor_position: None,
            selections: Vec::new(),
            line_ending: "\n".into(),
            is_dirty: self.is_dirty,
            timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        }
    }
}

fn worktree_root(abs_path: &std::path::Path, relative_component_count: usize) -> PathBuf {
    // A single file opened on its own is its own worktree, with an empty relative path.
    let ancestor_index = relative_component_count.max(1);
    abs_path
        .ancestors()
        .nth(ancestor_index)
        .unwrap_or(abs_path)
        .to_path_buf()
}

/// Maps Zed's lowercased language names to the VS Code language identifiers the language server
/// expects, where they differ.
fn editor_language_id(lsp_id: &str) -> String {
    match lsp_id {
        "tsx" => "typescriptreact",
        "c++" => "cpp",
        "c#" => "csharp",
        "shell script" => "shellscript",
        lsp_id => lsp_id,
    }
    .to_string()
}

/// Maps Zed's lowercased language names to the language server's `Language` enum. Unknown values
/// are rejected by the server's request parser, so anything unmapped is sent as unspecified.
fn language_enum(lsp_id: &str) -> &'static str {
    match lsp_id {
        "c" => "LANGUAGE_C",
        "c++" => "LANGUAGE_CPP",
        "c#" => "LANGUAGE_CSHARP",
        "css" => "LANGUAGE_CSS",
        "dart" => "LANGUAGE_DART",
        "dockerfile" => "LANGUAGE_DOCKERFILE",
        "elixir" => "LANGUAGE_ELIXIR",
        "erlang" => "LANGUAGE_ERLANG",
        "go" => "LANGUAGE_GO",
        "haskell" => "LANGUAGE_HASKELL",
        "html" => "LANGUAGE_HTML",
        "java" => "LANGUAGE_JAVA",
        "javascript" => "LANGUAGE_JAVASCRIPT",
        "json" | "jsonc" => "LANGUAGE_JSON",
        "kotlin" => "LANGUAGE_KOTLIN",
        "lua" => "LANGUAGE_LUA",
        "markdown" => "LANGUAGE_MARKDOWN",
        "php" => "LANGUAGE_PHP",
        "plaintext" => "LANGUAGE_PLAINTEXT",
        "python" => "LANGUAGE_PYTHON",
        "ruby" => "LANGUAGE_RUBY",
        "rust" => "LANGUAGE_RUST",
        "scala" => "LANGUAGE_SCALA",
        "shell script" => "LANGUAGE_SHELL",
        "sql" => "LANGUAGE_SQL",
        "swift" => "LANGUAGE_SWIFT",
        "toml" => "LANGUAGE_TOML",
        "tsx" => "LANGUAGE_TSX",
        "typescript" => "LANGUAGE_TYPESCRIPT",
        "yaml" => "LANGUAGE_YAML",
        _ => "LANGUAGE_UNSPECIFIED",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_worktree_root() {
        let path = Path::new("/projects/zed/crates/devin/src/devin.rs");
        assert_eq!(worktree_root(path, 4), Path::new("/projects/zed"));
        assert_eq!(
            worktree_root(path, 0),
            Path::new("/projects/zed/crates/devin/src")
        );
    }
}

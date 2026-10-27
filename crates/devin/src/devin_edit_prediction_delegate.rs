use std::{ops::Range, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context as _, Result, anyhow};
use edit_prediction_types::{
    EditPrediction, EditPredictionDelegate, EditPredictionDiscardReason, EditPredictionIconSet,
    EditPredictionRequestTrigger, PredictedCursorPosition, interpolate_edits,
};
use gpui::{App, AppContext as _, Context, Entity, EntityId, SharedString, Task};
use icons::IconName;
use language::{Anchor, Buffer, BufferSnapshot, EditPreview, language_settings::LanguageSettings};
use text::ToOffset as _;

use crate::{
    Devin,
    protocol::{
        Document, DocumentRange, EditorOptions, Position, SupercompleteTriggerCondition,
        convert_diff, diff_targets_document, locate_new_offset,
    },
};

pub struct DevinEditPredictionDelegate {
    devin: Entity<Devin>,
    pending_request: Option<Task<Result<()>>>,
    current_prediction: Option<CurrentPrediction>,
}

struct CurrentPrediction {
    buffer_id: EntityId,
    completion_id: SharedString,
    snapshot: BufferSnapshot,
    edits: Arc<[(Range<Anchor>, Arc<str>)]>,
    cursor_position: Option<PredictedCursorPosition>,
    edit_preview: EditPreview,
}

impl CurrentPrediction {
    fn interpolate(&self, new_snapshot: &BufferSnapshot) -> Option<Vec<(Range<Anchor>, Arc<str>)>> {
        interpolate_edits(&self.snapshot, new_snapshot, &self.edits)
            .filter(|edits| !edits.is_empty())
    }
}

impl DevinEditPredictionDelegate {
    pub fn new(devin: Entity<Devin>) -> Self {
        Self {
            devin,
            pending_request: None,
            current_prediction: None,
        }
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
        EditPredictionIconSet::new(IconName::ZedPredict)
    }

    fn is_enabled(&self, buffer: &Entity<Buffer>, _cursor_position: Anchor, cx: &App) -> bool {
        self.devin.read(cx).is_signed_in()
            && buffer
                .read(cx)
                .file()
                .is_some_and(|file| file.as_local().is_some())
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

        let Some(document) = document_for_request(&buffer, &snapshot, cursor_position, cx) else {
            return;
        };
        let Some(server) = self
            .devin
            .update(cx, |devin, cx| devin.server(&document.workspace_uri, cx))
        else {
            return;
        };
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
        let buffer_id = buffer.entity_id();

        self.pending_request = Some(cx.spawn(async move |this, cx| {
            if !debounce_duration.is_zero() {
                cx.background_executor().timer(debounce_duration).await;
            }

            let result = async {
                let server = server.await.map_err(|error| anyhow!("{error}"))?;
                let request_text = document.text.clone();
                let document_uri = document.absolute_uri.clone();
                let response = cx
                    .background_spawn(async move {
                        server
                            .complete(document, editor_options, trigger_condition)
                            .await
                    })
                    .await?;

                let Some(diff) = response.diff else {
                    if response.tab_jump.is_some() {
                        log::debug!("Devin returned a jump suggestion, which isn't supported yet");
                    } else {
                        log::debug!(
                            "Devin returned no edit (stop reason: {:?}, filter reason: {:?})",
                            response.stop_reason,
                            response.filter_reason
                        );
                    }
                    return Ok(None);
                };
                if !diff_targets_document(&diff.path, &document_uri) {
                    log::debug!(
                        "Devin suggested an edit in another file, which isn't supported yet"
                    );
                    return Ok(None);
                }
                let converted = cx
                    .background_spawn(async move { convert_diff(&request_text, &diff) })
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
                            snapshot.anchor_after(range.start)..snapshot.anchor_before(range.end),
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
                        None => {
                            PredictedCursorPosition::at_anchor(snapshot.anchor_before(old_offset))
                        }
                    }
                });
                let edit_preview = buffer
                    .read_with(cx, |buffer, cx| buffer.preview_edits(edits.clone(), cx))
                    .await;

                anyhow::Ok(Some(CurrentPrediction {
                    buffer_id,
                    completion_id: response.completion_id.into(),
                    snapshot,
                    edits,
                    cursor_position,
                    edit_preview,
                }))
            }
            .await;

            this.update(cx, |this, cx| {
                this.pending_request = None;
                match result {
                    Ok(prediction) => this.current_prediction = prediction,
                    Err(error) => {
                        log::error!("Devin edit prediction failed: {error:#}");
                        this.current_prediction = None;
                    }
                }
                cx.notify();
            })
        }));
    }

    fn accept(&mut self, _cx: &mut Context<Self>) {
        self.pending_request = None;
        self.current_prediction = None;
    }

    fn discard(&mut self, _reason: EditPredictionDiscardReason, _cx: &mut Context<Self>) {
        self.pending_request = None;
        self.current_prediction = None;
    }

    fn suggest(
        &mut self,
        buffer: &Entity<Buffer>,
        _cursor_position: Anchor,
        cx: &mut Context<Self>,
    ) -> Option<EditPrediction> {
        let current_prediction = self.current_prediction.as_ref()?;
        if current_prediction.buffer_id != buffer.entity_id() {
            return None;
        }
        let snapshot = buffer.read(cx).snapshot();
        let edits = current_prediction.interpolate(&snapshot)?;
        // The predicted cursor is only meaningful for the edits exactly as they were predicted.
        let unchanged = snapshot.version == current_prediction.snapshot.version;
        Some(EditPrediction::Local {
            id: Some(current_prediction.completion_id.clone()),
            edits,
            cursor_position: current_prediction.cursor_position.filter(|_| unchanged),
            edit_preview: Some(current_prediction.edit_preview.clone()),
        })
    }
}

fn document_for_request(
    buffer: &Entity<Buffer>,
    snapshot: &BufferSnapshot,
    cursor_position: Anchor,
    cx: &App,
) -> Option<Document> {
    let buffer = buffer.read(cx);
    let file = buffer.file()?;
    let abs_path = file.as_local()?.abs_path(cx);
    let worktree_root = worktree_root(&abs_path, file.path().components().count());
    let absolute_uri = url::Url::from_file_path(&abs_path).ok()?.to_string();
    let workspace_uri = url::Url::from_directory_path(&worktree_root)
        .ok()?
        .to_string();

    let cursor_offset = cursor_position.to_offset(snapshot);
    let cursor_point = snapshot.offset_to_point(cursor_offset);
    let cursor = Position {
        row: u64::from(cursor_point.row),
        col: u64::from(cursor_point.column),
    };

    let language_id = snapshot
        .language()
        .map(|language| language.lsp_id())
        .unwrap_or_default();
    Some(Document {
        absolute_uri,
        workspace_uri,
        // Zed buffers store LF line endings internally, so send the text and diff it in that
        // form, rather than the line ending used on disk.
        text: snapshot.text(),
        editor_language: if language_id.is_empty() {
            "plaintext".to_string()
        } else {
            editor_language_id(&language_id)
        },
        language: Some(language_enum(&language_id)),
        cursor_offset: cursor_offset as u64,
        cursor_position: Some(cursor.clone()),
        selections: vec![DocumentRange {
            start_position: cursor.clone(),
            end_position: cursor,
        }],
        line_ending: "\n".into(),
        is_dirty: buffer.is_dirty(),
        timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })
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

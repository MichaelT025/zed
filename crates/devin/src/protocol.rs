use std::ops::Range;

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Deserializer, Serialize};

pub const LANGUAGE_SERVER_SERVICE: &str = "/exa.language_server_pb.LanguageServerService/";
pub const EXTENSION_SERVER_SERVICE: &str = "/exa.extension_server_pb.ExtensionServerService/";
pub const SEAT_MANAGEMENT_SERVICE: &str = "/exa.seat_management_pb.SeatManagementService/";

#[derive(Clone, Debug, Serialize)]
pub struct Metadata {
    pub ide_name: String,
    pub ide_version: String,
    pub extension_name: String,
    pub extension_version: String,
    pub extension_path: String,
    pub api_key: String,
    pub session_id: String,
    pub request_id: String,
    pub locale: String,
    pub os: String,
    pub disable_telemetry: bool,
    pub plan_name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub team_id: String,
}

impl Metadata {
    /// Encodes the metadata as the protobuf message the language server reads from stdin when
    /// launched with `--stdin_initial_metadata`.
    pub fn to_protobuf(&self) -> Vec<u8> {
        let mut output = Vec::new();
        // Field numbers come from the `Metadata` descriptor embedded in the language server.
        encode_string_field(&mut output, 1, &self.ide_name);
        encode_string_field(&mut output, 2, &self.extension_version);
        encode_string_field(&mut output, 3, &self.api_key);
        encode_string_field(&mut output, 4, &self.locale);
        encode_string_field(&mut output, 5, &self.os);
        encode_varint_field(&mut output, 6, self.disable_telemetry as u64);
        encode_string_field(&mut output, 7, &self.ide_version);
        if let Ok(request_id) = self.request_id.parse::<u64>() {
            encode_varint_field(&mut output, 9, request_id);
        }
        encode_string_field(&mut output, 10, &self.session_id);
        encode_string_field(&mut output, 12, &self.extension_name);
        encode_string_field(&mut output, 17, &self.extension_path);
        encode_string_field(&mut output, 26, &self.plan_name);
        if !self.team_id.is_empty() {
            encode_string_field(&mut output, 32, &self.team_id);
        }
        output
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Position {
    #[serde(serialize_with = "serialize_u64_string")]
    pub row: u64,
    #[serde(serialize_with = "serialize_u64_string")]
    pub col: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct DocumentRange {
    pub start_position: Position,
    pub end_position: Position,
}

#[derive(Clone, Debug, Serialize)]
pub struct Document {
    pub absolute_uri: String,
    pub workspace_uri: String,
    pub text: String,
    pub editor_language: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<&'static str>,
    #[serde(serialize_with = "serialize_u64_string")]
    pub cursor_offset: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor_position: Option<Position>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub selections: Vec<DocumentRange>,
    pub line_ending: String,
    pub is_dirty: bool,
    pub timestamp: String,
}

impl Document {
    /// Returns a copy of the document with the cursor at `cursor_offset`, a byte offset into its
    /// text on a character boundary.
    pub fn with_cursor(&self, cursor_offset: usize) -> Self {
        let before_cursor = &self.text[..cursor_offset];
        let line_start = before_cursor
            .rfind('\n')
            .map_or(0, |newline_offset| newline_offset + 1);
        let cursor = Position {
            row: before_cursor.matches('\n').count() as u64,
            col: (cursor_offset - line_start) as u64,
        };
        Self {
            cursor_offset: cursor_offset as u64,
            cursor_position: Some(cursor.clone()),
            selections: vec![DocumentRange {
                start_position: cursor.clone(),
                end_position: cursor,
            }],
            ..self.clone()
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct EditorOptions {
    #[serde(serialize_with = "serialize_u64_string")]
    pub tab_size: u64,
    pub insert_spaces: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum SupercompleteTriggerCondition {
    #[serde(rename = "SUPERCOMPLETE_TRIGGER_CONDITION_TYPING")]
    Typing,
    #[serde(rename = "SUPERCOMPLETE_TRIGGER_CONDITION_CURSOR_LINE_NAVIGATION")]
    CursorLineNavigation,
    #[serde(rename = "SUPERCOMPLETE_TRIGGER_CONDITION_FORCED")]
    Forced,
    #[serde(rename = "SUPERCOMPLETE_TRIGGER_CONDITION_SUPERCOMPLETE_ACCEPT")]
    SupercompleteAccept,
}

#[derive(Clone, Debug, Serialize)]
pub struct HandleStreamingTabV2Request {
    pub metadata: Metadata,
    pub document: Document,
    pub editor_options: EditorOptions,
    pub supercomplete_trigger_condition: SupercompleteTriggerCondition,
    pub other_documents: Vec<Document>,
    pub disable_supercomplete: bool,
    pub disable_tab_jump: bool,
    pub supercomplete_aggression: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum IdeAction {
    #[serde(rename = "IDE_ACTION_STARTUP")]
    Startup,
    #[serde(rename = "IDE_ACTION_ACTIVE_EDITOR_CHANGED")]
    ActiveEditorChanged,
}

#[derive(Clone, Debug, Serialize)]
pub struct RefreshContextForIdeActionRequest {
    pub metadata: Metadata,
    pub active_document: Document,
    pub other_documents: Vec<Document>,
    pub recently_opened_uris: Vec<String>,
    pub blocking: bool,
    pub ide_action: IdeAction,
}

#[derive(Clone, Debug, Serialize)]
pub struct MetadataRequest {
    pub metadata: Metadata,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HandleStreamingTabV2Response {
    pub completion_id: String,
    pub prompt_id: String,
    pub request_uid: String,
    pub diff: Option<SupercompleteDiff>,
    pub tab_jump: Option<TabJump>,
    pub noop: Option<serde_json::Value>,
    pub stop_reason: Option<serde_json::Value>,
    pub filter_reason: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SupercompleteDiff {
    pub path: String,
    #[serde(deserialize_with = "deserialize_u64_string")]
    pub selection_start_line: u64,
    #[serde(deserialize_with = "deserialize_u64_string")]
    pub selection_end_line: u64,
    pub character_diff: Option<CharacterDiff>,
    pub cursor_position: Option<ResponsePosition>,
    pub old_str: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TabJump {
    pub path: String,
    pub jump_position: Option<ResponsePosition>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct CharacterDiff {
    pub changes: Vec<CharacterDiffChange>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct CharacterDiffChange {
    pub text: String,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ResponsePosition {
    #[serde(deserialize_with = "deserialize_u64_string")]
    pub row: u64,
    #[serde(deserialize_with = "deserialize_u64_string")]
    pub col: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GetUserStatusResponse {
    pub user_status: Option<serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExchangeDevinCodeResponse {
    pub session_token: String,
}

fn serialize_u64_string<S: serde::Serializer>(
    value: &u64,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    // Protobuf JSON represents 64-bit integers as strings.
    serializer.serialize_str(&value.to_string())
}

fn deserialize_u64_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrNumber {
        String(String),
        Number(u64),
    }

    match StringOrNumber::deserialize(deserializer)? {
        StringOrNumber::String(string) => string.parse().map_err(serde::de::Error::custom),
        StringOrNumber::Number(number) => Ok(number),
    }
}

fn encode_varint(output: &mut Vec<u8>, mut value: u64) {
    while value > 0x7f {
        output.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

fn encode_varint_field(output: &mut Vec<u8>, field_number: u64, value: u64) {
    encode_varint(output, field_number << 3);
    encode_varint(output, value);
}

fn encode_bytes_field(output: &mut Vec<u8>, field_number: u64, value: &[u8]) {
    encode_varint(output, (field_number << 3) | 2);
    encode_varint(output, value.len() as u64);
    output.extend_from_slice(value);
}

fn encode_string_field(output: &mut Vec<u8>, field_number: u64, value: &str) {
    encode_bytes_field(output, field_number, value.as_bytes());
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtobufValue {
    Varint(u64),
    Bytes(Vec<u8>),
}

/// Decodes the top level of a protobuf message into `(field_number, value)` pairs, skipping
/// fixed-width fields, which the callback messages handled here don't use.
pub fn decode_protobuf_fields(mut data: &[u8]) -> Result<Vec<(u64, ProtobufValue)>> {
    fn read_varint(data: &mut &[u8]) -> Result<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let (&byte, rest) = data.split_first().context("truncated varint")?;
            *data = rest;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        bail!("varint is too long")
    }

    let mut fields = Vec::new();
    while !data.is_empty() {
        let key = read_varint(&mut data)?;
        let field_number = key >> 3;
        match key & 7 {
            0 => fields.push((field_number, ProtobufValue::Varint(read_varint(&mut data)?))),
            1 => data = data.get(8..).context("truncated fixed64")?,
            2 => {
                let length = usize::try_from(read_varint(&mut data)?)?;
                let value = data
                    .get(..length)
                    .context("truncated length-delimited field")?;
                fields.push((field_number, ProtobufValue::Bytes(value.to_vec())));
                data = &data[length..];
            }
            5 => data = data.get(4..).context("truncated fixed32")?,
            wire_type => bail!("unsupported protobuf wire type {wire_type}"),
        }
    }
    Ok(fields)
}

pub fn encode_native_values_response(values: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut output = Vec::new();
    for (key, value) in values {
        let mut wrapped_value = Vec::new();
        encode_bytes_field(&mut wrapped_value, 1, value);
        let mut entry = Vec::new();
        encode_string_field(&mut entry, 1, key);
        encode_bytes_field(&mut entry, 2, &wrapped_value);
        encode_bytes_field(&mut output, 1, &entry);
    }
    output
}

pub fn encode_set_native_value_response(conflict: bool, current: Option<&[u8]>) -> Vec<u8> {
    let mut output = Vec::new();
    encode_varint_field(&mut output, 1, conflict as u64);
    if conflict && let Some(current) = current {
        let mut wrapped_value = Vec::new();
        encode_bytes_field(&mut wrapped_value, 1, current);
        encode_bytes_field(&mut output, 2, &wrapped_value);
    }
    output
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConvertedDiff {
    /// Byte ranges in the request text, paired with their replacement text, in ascending order.
    pub edits: Vec<(Range<usize>, String)>,
    /// The predicted cursor as an offset into the post-edit text.
    pub cursor_offset: Option<usize>,
}

/// Converts a response diff into edits against `text`, the exact text that was sent in the request.
///
/// The language server returns LF-normalized strings and its line bounds don't follow a single
/// convention, so the old text is validated against both the character diff and the request text
/// at the start line, rather than trusting the reported end line. Anything inconsistent is
/// rejected instead of being fuzzily relocated.
pub fn convert_diff(text: &str, diff: &SupercompleteDiff) -> Result<ConvertedDiff> {
    let changes = diff
        .character_diff
        .as_ref()
        .map(|character_diff| character_diff.changes.as_slice())
        .filter(|changes| !changes.is_empty())
        .context("diff has no character changes")?;

    let mut old_from_changes = String::new();
    let mut new_from_changes = String::new();
    for change in changes {
        let change_text = normalize_line_endings(&change.text);
        match change.kind.as_str() {
            "DIFF_CHANGE_TYPE_UNCHANGED" => {
                old_from_changes.push_str(&change_text);
                new_from_changes.push_str(&change_text);
            }
            "DIFF_CHANGE_TYPE_DELETE" => old_from_changes.push_str(&change_text),
            "DIFF_CHANGE_TYPE_INSERT" => new_from_changes.push_str(&change_text),
            kind => bail!("unknown character diff change type {kind:?}"),
        }
    }

    let old_str = normalize_line_endings(&diff.old_str);
    if old_str != old_from_changes {
        bail!("old text does not match the character diff");
    }

    let region_start = line_start_offset(text, diff.selection_start_line)
        .with_context(|| format!("start line {} is out of range", diff.selection_start_line))?;
    if !text[region_start..].starts_with(old_str.as_str()) {
        bail!("old text does not match the request text");
    }

    let mut edits = Vec::new();
    let mut offset = region_start;
    let mut pending: Option<(Range<usize>, String)> = None;
    for change in changes {
        let change_text = normalize_line_endings(&change.text);
        match change.kind.as_str() {
            "DIFF_CHANGE_TYPE_UNCHANGED" => {
                edits.extend(pending.take());
                offset += change_text.len();
            }
            "DIFF_CHANGE_TYPE_DELETE" => {
                let (range, _) = pending.get_or_insert_with(|| (offset..offset, String::new()));
                offset += change_text.len();
                range.end = offset;
            }
            "DIFF_CHANGE_TYPE_INSERT" => {
                let (_, new_text) = pending.get_or_insert_with(|| (offset..offset, String::new()));
                new_text.push_str(&change_text);
            }
            _ => {}
        }
    }
    edits.extend(pending.take());
    edits.retain(|(range, new_text)| !range.is_empty() || !new_text.is_empty());

    let cursor_offset = diff.cursor_position.and_then(|position| {
        let new_region_offset = offset_for_position(
            &new_from_changes,
            position.row.checked_sub(diff.selection_start_line)?,
            position.col,
        )?;
        Some(region_start + new_region_offset)
    });

    Ok(ConvertedDiff {
        edits,
        cursor_offset,
    })
}

/// Maps an offset in the post-edit text to an offset in the original text, plus an offset into
/// the inserted text when it falls inside an edit.
pub fn locate_new_offset(edits: &[(Range<usize>, String)], new_offset: usize) -> (usize, usize) {
    let mut delta: isize = 0;
    for (range, new_text) in edits {
        let new_start = range.start.saturating_add_signed(delta);
        if new_offset < new_start {
            break;
        }
        if new_offset <= new_start + new_text.len() {
            return (range.start, new_offset - new_start);
        }
        delta += new_text.len() as isize - range.len() as isize;
    }
    (new_offset.saturating_add_signed(-delta), 0)
}

/// Returns the offset in `text` of a jump target. The column is clamped to its line, because the
/// position only needs to land on the line the server wants the cursor on.
pub fn jump_offset(text: &str, position: ResponsePosition) -> Option<usize> {
    let line_start = line_start_offset(text, position.row)?;
    let line_end = text[line_start..]
        .find('\n')
        .map_or(text.len(), |newline_offset| line_start + newline_offset);
    let mut offset = usize::try_from(position.col)
        .ok()
        .and_then(|column| line_start.checked_add(column))
        .map_or(line_end, |offset| offset.min(line_end));
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    Some(offset)
}

/// Returns whether a diff's path refers to the requested document. The server may format file
/// URIs differently from the request (for example `file:///c%3A/...` versus `file:///C:/...`), so
/// they're compared as file paths. An empty path is treated as the requested document.
pub fn diff_targets_document(diff_path: &str, document_uri: &str) -> bool {
    if diff_path.is_empty() || diff_path == document_uri {
        return true;
    }
    let file_path = |uri: &str| {
        let path = url::Url::parse(uri).ok()?.to_file_path().ok()?;
        let path = path.to_string_lossy().into_owned();
        Some(if cfg!(windows) {
            path.to_lowercase()
        } else {
            path
        })
    };
    match (file_path(diff_path), file_path(document_uri)) {
        (Some(diff_path), Some(document_path)) => diff_path == document_path,
        _ => false,
    }
}

fn normalize_line_endings(text: &str) -> String {
    let mut text = text.to_string();
    text::LineEnding::normalize(&mut text);
    text
}

fn line_start_offset(text: &str, row: u64) -> Option<usize> {
    if row == 0 {
        return Some(0);
    }
    let row = usize::try_from(row).ok()?;
    text.match_indices('\n')
        .nth(row - 1)
        .map(|(newline_offset, _)| newline_offset + 1)
}

fn offset_for_position(text: &str, row: u64, column: u64) -> Option<usize> {
    let line_start = line_start_offset(text, row)?;
    let line_end = text[line_start..]
        .find('\n')
        .map_or(text.len(), |newline_offset| line_start + newline_offset);
    let offset = line_start.checked_add(usize::try_from(column).ok()?)?;
    (offset <= line_end && text.is_char_boundary(offset)).then_some(offset)
}

pub fn parse_language_server_started(body: &[u8], binary: bool) -> Result<(u16, u16)> {
    let (port, lsp_port) = if binary {
        let fields = decode_protobuf_fields(body)?;
        let varint_field = |number: u64| {
            fields.iter().find_map(|(field_number, value)| match value {
                ProtobufValue::Varint(value) if *field_number == number => Some(*value),
                _ => None,
            })
        };
        (varint_field(1).unwrap_or(0), varint_field(2).unwrap_or(0))
    } else {
        let value: serde_json::Value = serde_json::from_slice(body)?;
        let number_field = |camel_case: &str, snake_case: &str| {
            value
                .get(camel_case)
                .or_else(|| value.get(snake_case))
                .and_then(|value| {
                    value
                        .as_u64()
                        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
                })
                .unwrap_or(0)
        };
        (
            number_field("languageServerPort", "language_server_port"),
            number_field("lspPort", "lsp_port"),
        )
    };
    let port = u16::try_from(port)
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| anyhow!("invalid language server port {port}"))?;
    Ok((port, u16::try_from(lsp_port).unwrap_or(0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn response_diff(json: serde_json::Value) -> SupercompleteDiff {
        serde_json::from_value::<HandleStreamingTabV2Response>(serde_json::json!({ "diff": json }))
            .unwrap()
            .diff
            .unwrap()
    }

    fn apply(text: &str, edits: &[(Range<usize>, String)]) -> String {
        let mut result = text.to_string();
        for (range, new_text) in edits.iter().rev() {
            result.replace_range(range.clone(), new_text);
        }
        result
    }

    // Observed response for the LF fixture recorded in the windsurf-zed research repository.
    #[test]
    fn test_observed_insertion_at_end_of_line() {
        let text = "def add(a, b):\n    return a + b\n\ndef multiply(a, b):\n    return ";
        let diff = response_diff(serde_json::json!({
            "path": "file:///synthetic/example.py",
            "selectionEndLine": "5",
            "characterDiff": { "changes": [
                { "text": text, "type": "DIFF_CHANGE_TYPE_UNCHANGED" },
                { "text": "a * b", "type": "DIFF_CHANGE_TYPE_INSERT" }
            ]},
            "cursorPosition": { "row": "4", "col": "16" },
            "oldStr": text
        }));

        let converted = convert_diff(text, &diff).unwrap();
        assert_eq!(
            converted.edits,
            vec![(text.len()..text.len(), "a * b".into())]
        );
        let new_text = apply(text, &converted.edits);
        assert_eq!(
            new_text,
            "def add(a, b):\n    return a + b\n\ndef multiply(a, b):\n    return a * b"
        );
        assert_eq!(converted.cursor_offset, Some(new_text.len()));
        assert_eq!(
            locate_new_offset(&converted.edits, new_text.len()),
            (text.len(), 5)
        );
    }

    #[test]
    fn test_observed_unicode_insertion() {
        let text = "# caf\u{e9} \u{1f600}\ndef add(a, b):\n    return a + b\n\ndef multiply(a, b):\n    return ";
        let diff = response_diff(serde_json::json!({
            "selectionEndLine": "6",
            "characterDiff": { "changes": [
                { "text": text, "type": "DIFF_CHANGE_TYPE_UNCHANGED" },
                { "text": "a * b", "type": "DIFF_CHANGE_TYPE_INSERT" }
            ]},
            "cursorPosition": { "row": "5", "col": "16" },
            "oldStr": text
        }));

        let converted = convert_diff(text, &diff).unwrap();
        assert_eq!(
            converted.edits,
            vec![(text.len()..text.len(), "a * b".into())]
        );
        assert_eq!(converted.cursor_offset, Some(text.len() + 5));
    }

    #[test]
    fn test_observed_multiline_insertion_at_eof() {
        let text = "def add(a, b):\n    return a + b\n\nresult = add(2, ";
        let diff = response_diff(serde_json::json!({
            "selectionEndLine": "4",
            "characterDiff": { "changes": [
                { "text": text, "type": "DIFF_CHANGE_TYPE_UNCHANGED" },
                { "text": "3)\nprint(result)", "type": "DIFF_CHANGE_TYPE_INSERT" }
            ]},
            "cursorPosition": { "row": "4", "col": "13" },
            "oldStr": text
        }));

        let converted = convert_diff(text, &diff).unwrap();
        let new_text = apply(text, &converted.edits);
        assert_eq!(
            new_text,
            "def add(a, b):\n    return a + b\n\nresult = add(2, 3)\nprint(result)"
        );
        assert_eq!(converted.cursor_offset, Some(new_text.len()));
    }

    #[test]
    fn test_crlf_response_text_is_normalized() {
        let text = "a\nb\n";
        let diff = response_diff(serde_json::json!({
            "characterDiff": { "changes": [
                { "text": "a\r\n", "type": "DIFF_CHANGE_TYPE_UNCHANGED" },
                { "text": "x\r\n", "type": "DIFF_CHANGE_TYPE_INSERT" },
                { "text": "b\r\n", "type": "DIFF_CHANGE_TYPE_UNCHANGED" }
            ]},
            "oldStr": "a\r\nb\r\n"
        }));

        let converted = convert_diff(text, &diff).unwrap();
        assert_eq!(converted.edits, vec![(2..2, "x\n".into())]);
    }

    #[test]
    fn test_replacement_within_region_starting_at_later_line() {
        let text = "fn main() {\n    let x = 1;\n    println!(\"{}\", y);\n}\n";
        let diff = response_diff(serde_json::json!({
            "selectionStartLine": "2",
            "selectionEndLine": "3",
            "characterDiff": { "changes": [
                { "text": "    println!(\"{}\", ", "type": "DIFF_CHANGE_TYPE_UNCHANGED" },
                { "text": "y", "type": "DIFF_CHANGE_TYPE_DELETE" },
                { "text": "x", "type": "DIFF_CHANGE_TYPE_INSERT" },
                { "text": ");\n", "type": "DIFF_CHANGE_TYPE_UNCHANGED" }
            ]},
            "cursorPosition": { "row": 2, "col": 20 },
            "oldStr": "    println!(\"{}\", y);\n"
        }));

        let converted = convert_diff(text, &diff).unwrap();
        let y_offset = text.find('y').unwrap();
        assert_eq!(converted.edits, vec![(y_offset..y_offset + 1, "x".into())]);
        assert_eq!(
            apply(text, &converted.edits),
            "fn main() {\n    let x = 1;\n    println!(\"{}\", x);\n}\n"
        );
        assert_eq!(converted.cursor_offset, Some(y_offset + 1));
        assert_eq!(
            locate_new_offset(&converted.edits, y_offset + 1),
            (y_offset, 1)
        );
    }

    #[test]
    fn test_rejects_missing_character_diff() {
        let diff = response_diff(serde_json::json!({ "oldStr": "a" }));
        assert!(convert_diff("a", &diff).is_err());
    }

    #[test]
    fn test_rejects_old_text_that_disagrees_with_character_diff() {
        let diff = response_diff(serde_json::json!({
            "characterDiff": { "changes": [
                { "text": "a", "type": "DIFF_CHANGE_TYPE_UNCHANGED" },
                { "text": "b", "type": "DIFF_CHANGE_TYPE_INSERT" }
            ]},
            "oldStr": "z"
        }));
        assert!(convert_diff("a", &diff).is_err());
    }

    #[test]
    fn test_rejects_buffer_that_changed_since_request() {
        let diff = response_diff(serde_json::json!({
            "characterDiff": { "changes": [
                { "text": "let a", "type": "DIFF_CHANGE_TYPE_UNCHANGED" },
                { "text": " = 1;", "type": "DIFF_CHANGE_TYPE_INSERT" }
            ]},
            "oldStr": "let a"
        }));
        assert!(convert_diff("let b", &diff).is_err());
    }

    #[test]
    fn test_rejects_out_of_range_start_line() {
        let diff = response_diff(serde_json::json!({
            "selectionStartLine": "3",
            "characterDiff": { "changes": [
                { "text": "x", "type": "DIFF_CHANGE_TYPE_INSERT" }
            ]}
        }));
        assert!(convert_diff("a\nb", &diff).is_err());
    }

    #[test]
    fn test_ignores_cursor_outside_new_region() {
        let diff = response_diff(serde_json::json!({
            "selectionStartLine": "1",
            "characterDiff": { "changes": [
                { "text": "b", "type": "DIFF_CHANGE_TYPE_UNCHANGED" },
                { "text": "c", "type": "DIFF_CHANGE_TYPE_INSERT" }
            ]},
            "cursorPosition": { "row": "0", "col": "0" },
            "oldStr": "b"
        }));
        let converted = convert_diff("a\nb", &diff).unwrap();
        assert_eq!(converted.edits, vec![(3..3, "c".into())]);
        assert_eq!(converted.cursor_offset, None);
    }

    #[test]
    fn test_diff_targets_document() {
        assert!(diff_targets_document("", "file:///home/user/a.rs"));
        assert!(diff_targets_document(
            "file:///home/user/a.rs",
            "file:///home/user/a.rs"
        ));
        assert!(!diff_targets_document(
            "file:///home/user/b.rs",
            "file:///home/user/a.rs"
        ));
        assert!(!diff_targets_document(
            "not a uri",
            "file:///home/user/a.rs"
        ));
        if cfg!(windows) {
            assert!(diff_targets_document(
                "file:///c%3A/Users/me/a.rs",
                "file:///C:/Users/me/a.rs"
            ));
        }
    }

    #[test]
    fn test_locate_new_offset_before_and_after_edits() {
        let edits = vec![(2..4, "xyz".to_string()), (8..8, "!".to_string())];
        assert_eq!(locate_new_offset(&edits, 1), (1, 0));
        assert_eq!(locate_new_offset(&edits, 3), (2, 1));
        assert_eq!(locate_new_offset(&edits, 6), (5, 0));
        assert_eq!(locate_new_offset(&edits, 10), (8, 1));
        assert_eq!(locate_new_offset(&edits, 12), (10, 0));
    }

    #[test]
    fn test_document_with_cursor() {
        let document = Document {
            absolute_uri: "file:///synthetic/example.rs".into(),
            workspace_uri: "file:///synthetic/".into(),
            text: "fn main() {\n    let é = 1;\n}".into(),
            editor_language: "rust".into(),
            language: Some("LANGUAGE_RUST"),
            cursor_offset: 0,
            cursor_position: None,
            selections: Vec::new(),
            line_ending: "\n".into(),
            is_dirty: false,
            timestamp: String::new(),
        };
        let with_cursor = document.with_cursor(22);
        assert_eq!(with_cursor.cursor_offset, 22);
        let cursor = with_cursor.cursor_position.unwrap();
        assert_eq!((cursor.row, cursor.col), (1, 10));
        assert_eq!(with_cursor.selections.len(), 1);
        let start = document.with_cursor(0).cursor_position.unwrap();
        assert_eq!((start.row, start.col), (0, 0));
    }

    #[test]
    fn test_jump_offset() {
        let text = "fn main() {\n    let é = 1;\n}";
        let position = |row, col| ResponsePosition { row, col };
        assert_eq!(jump_offset(text, position(1, 4)), Some(16));
        // Past the end of the line, and inside a multi-byte character.
        assert_eq!(jump_offset(text, position(0, 50)), Some(11));
        assert_eq!(jump_offset(text, position(1, 9)), Some(20));
        assert_eq!(jump_offset(text, position(2, 0)), Some(28));
        assert_eq!(jump_offset(text, position(3, 0)), None);
    }

    #[test]
    fn test_parse_tab_jump_response() {
        let response: HandleStreamingTabV2Response = serde_json::from_value(serde_json::json!({
            "completionId": "completion",
            "tabJump": {
                "path": "file:///synthetic/example.py",
                "jumpPosition": { "row": "12", "col": "4" },
                "isImport": false
            }
        }))
        .unwrap();
        let tab_jump = response.tab_jump.unwrap();
        assert_eq!(tab_jump.path, "file:///synthetic/example.py");
        let jump_position = tab_jump.jump_position.unwrap();
        assert_eq!((jump_position.row, jump_position.col), (12, 4));
    }

    #[test]
    fn test_metadata_protobuf_round_trip() {
        let metadata = Metadata {
            ide_name: "windsurf".into(),
            ide_version: "3.10.67".into(),
            extension_name: "windsurf".into(),
            extension_version: "1.48.2".into(),
            extension_path: "C:\\path".into(),
            api_key: "key".into(),
            session_id: "session".into(),
            request_id: "300".into(),
            locale: "en".into(),
            os: "windows".into(),
            disable_telemetry: true,
            plan_name: "Unset".into(),
            team_id: String::new(),
        };
        let fields = decode_protobuf_fields(&metadata.to_protobuf()).unwrap();
        assert!(fields.contains(&(1, ProtobufValue::Bytes(b"windsurf".to_vec()))));
        assert!(fields.contains(&(3, ProtobufValue::Bytes(b"key".to_vec()))));
        assert!(fields.contains(&(6, ProtobufValue::Varint(1))));
        assert!(fields.contains(&(9, ProtobufValue::Varint(300))));
        assert!(!fields.iter().any(|(number, _)| *number == 32));
    }

    #[test]
    fn test_parse_language_server_started() {
        let mut binary = Vec::new();
        encode_varint_field(&mut binary, 1, 51234);
        encode_varint_field(&mut binary, 2, 51235);
        encode_string_field(&mut binary, 4, "csrf");
        assert_eq!(
            parse_language_server_started(&binary, true).unwrap(),
            (51234, 51235)
        );
        assert_eq!(
            parse_language_server_started(
                br#"{"languageServerPort":4000,"lspPort":"4001"}"#,
                false
            )
            .unwrap(),
            (4000, 4001)
        );
        assert!(parse_language_server_started(b"{}", false).is_err());
    }
}

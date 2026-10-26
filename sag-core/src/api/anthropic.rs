//! Typed serde DTOs for the Anthropic Messages API (`POST /v1/messages`).
//!
//! Field names keep their original JSON spelling (snake_case). Three rules
//! shape every type in this module:
//!
//! 1. **No `deny_unknown_fields`.** Every struct carries a flattened
//!    [`Extra`] map, so fields this module does not model survive a
//!    deserialize → serialize round-trip untouched.
//! 2. **Discriminators are plain `String`s.** API body enum values
//!    (`type`, `role`, `stop_reason`, …) are never strictly validated: an
//!    unknown value must be forwarded, not rejected (see the gateway design
//!    principle "rejecting unknown values is an anti-pattern" — strictness is
//!    reserved for gateway *config*).
//! 3. **Every union has a lossless `Unknown(Value)` fallback.** Untagged
//!    unions that are structurally ambiguous (several variants sharing the
//!    same shape, e.g. `tool_choice: auto | any | none`, or
//!    `thinking: adaptive | disabled`) dispatch on their `type` discriminator
//!    instead of relying on variant order; everything unrecognized — unknown
//!    tags *and* non-object payloads — lands in `Unknown` verbatim.
//!
//! Note on `temperature` / `top_p` / `top_k`: Anthropic marks them deprecated
//! for models released after Claude Opus 4.6. They stay typed here (rather
//! than falling into `extra`) because they are live protocol-translation
//! inputs — `chat ingress → IR sampling → anthropic upstream` — and older
//! models still accept them.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Catch-all bucket for JSON fields not modelled by the DTO owning it.
pub type Extra = Map<String, Value>;

/// Parse a buffered JSON value into a concrete DTO.
fn parse<T: serde::de::DeserializeOwned>(value: &Value) -> Result<T, serde_json::Error> {
    T::deserialize(value)
}

/// Deserialize a `type`-discriminated union without rejecting unknown values.
///
/// `convert` is offered the `type` string plus the buffered payload and
/// returns `None` for tags it does not know; the payload is then handed to
/// `unknown` (the `Unknown(Value)` variant) exactly as received. A known tag
/// with a malformed payload is a genuine deserialization error (callers turn
/// it into a 422 `invalid_request_error`), matching the strictness the typed
/// chat path already has for known fields.
fn deserialize_tagged<'de, D, T>(
    deserializer: D,
    convert: impl FnOnce(&str, &Value) -> Option<Result<T, serde_json::Error>>,
    unknown: impl FnOnce(Value) -> T,
) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    let tag = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match convert(tag, &value) {
        Some(result) => result.map_err(serde::de::Error::custom),
        None => Ok(unknown(value)),
    }
}

// ═══════════════════════════════════════════════════════════════════
// Shared primitives
// ═══════════════════════════════════════════════════════════════════

/// `cache_control` breakpoint marker (`{"type": "ephemeral"}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheControlEphemeral {
    /// Constant `"ephemeral"`; kept as a `String` (never validated).
    #[serde(rename = "type")]
    pub kind: String,
    /// Cache lifetime: `"5m"` (default) or `"1h"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Request `metadata` object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicMetadata {
    /// Opaque end-user identifier used for abuse detection (max 512 chars).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `document.citations` switch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CitationsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// One citation attached to a `text` block.
///
/// The five documented location variants (`char_location`, `page_location`,
/// `content_block_location`, `web_search_result_location`,
/// `search_result_location`) are display metadata forwarded verbatim, so only
/// the discriminator is typed and the location fields ride in `extra`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextCitation {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Caller identity of a `tool_use` / `server_tool_use` block.
///
/// `{"type": "direct"}` for client tools, `{"type": "<versioned>", "tool_id":
/// …}` when the call was issued by a server tool (e.g. code execution).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCaller {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_id: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

// ═══════════════════════════════════════════════════════════════════
// System prompt
// ═══════════════════════════════════════════════════════════════════

/// Top-level `system` prompt: either a bare string or text blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SystemPrompt {
    /// `"system": "you are …"`
    Text(String),
    /// `"system": [{"type": "text", "text": "…", "cache_control": {…}}]`
    Blocks(Vec<TextBlock>),
}

// ═══════════════════════════════════════════════════════════════════
// Content sources (image / document)
// ═══════════════════════════════════════════════════════════════════

/// Inline base64 payload (`{"type": "base64", …}`) shared by image and
/// document sources.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Base64Source {
    /// Constant `"base64"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// e.g. `"image/png"`, `"application/pdf"`.
    pub media_type: String,
    pub data: String,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Remote URL source (`{"type": "url", "url": …}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UrlSource {
    /// Constant `"url"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Previously uploaded file reference (`{"type": "file", "file_id": …}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSource {
    /// Constant `"file"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub file_id: String,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Inline plain text document (`{"type": "text", "data": …}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlainTextSource {
    /// Constant `"text"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// `"text/plain"` when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    pub data: String,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Inline content-block document (`{"type": "content", "content": …}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentBlockSource {
    /// Constant `"content"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub content: ContentBlockSourceContent,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Payload of a [`ContentBlockSource`]: plain text or nested blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ContentBlockSourceContent {
    Text(String),
    Blocks(Vec<AnthropicContentBlock>),
}

/// `image.source` union.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ImageSource {
    Base64(Base64Source),
    Url(UrlSource),
    File(FileSource),
    Unknown(Value),
}

impl<'de> Deserialize<'de> for ImageSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserialize_tagged(
            deserializer,
            |tag, value| {
                Some(match tag {
                    "base64" => parse(value).map(Self::Base64),
                    "url" => parse(value).map(Self::Url),
                    "file" => parse(value).map(Self::File),
                    _ => return None,
                })
            },
            Self::Unknown,
        )
    }
}

/// `document.source` union.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum DocumentSource {
    Base64(Base64Source),
    Text(PlainTextSource),
    Content(ContentBlockSource),
    Url(UrlSource),
    File(FileSource),
    Unknown(Value),
}

impl<'de> Deserialize<'de> for DocumentSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserialize_tagged(
            deserializer,
            |tag, value| {
                Some(match tag {
                    "base64" => parse(value).map(Self::Base64),
                    "text" => parse(value).map(Self::Text),
                    "content" => parse(value).map(Self::Content),
                    "url" => parse(value).map(Self::Url),
                    "file" => parse(value).map(Self::File),
                    _ => return None,
                })
            },
            Self::Unknown,
        )
    }
}

// ═══════════════════════════════════════════════════════════════════
// Content blocks
// ═══════════════════════════════════════════════════════════════════

/// `{"type": "text", "text": …}` — the most common block, also used by
/// `system` and `tool_result` content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextBlock {
    /// Constant `"text"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
    /// Citations the model produced for this text (response side).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub citations: Option<Vec<TextCitation>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControlEphemeral>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `{"type": "image", "source": …}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageBlock {
    /// Constant `"image"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub source: ImageSource,
    /// Image transformation hints. The documented shape is still evolving,
    /// so the object is carried verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transformations: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControlEphemeral>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `{"type": "document", "source": …}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentBlock {
    /// Constant `"document"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub source: DocumentSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub citations: Option<CitationsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControlEphemeral>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `{"type": "tool_use", "id": …, "name": …, "input": {…}}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUseBlock {
    /// Constant `"tool_use"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub name: String,
    /// Tool arguments. Documented as an object; replay loops that omit it still
    /// parse. `Option` keeps the three wire states apart so both directions are
    /// lossless:
    ///
    /// - absent upstream → `None` → the key is **not** emitted, so a body that
    ///   never carried `input` never grows one;
    /// - explicit `"input": {}` → `Some(empty)` → re-emitted verbatim as
    ///   `"input": {}`;
    /// - non-empty input → `Some(map)` → re-emitted unchanged.
    ///
    /// Collapsing the empty map into the default would make the second state
    /// indistinguishable from the first, and the round-trip guard in
    /// `parse_faithful` (`sag-providers/src/codec/anthropic.rs`) would then
    /// mistake an explicit empty object for a fabricated one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller: Option<ToolCaller>,
    /// Set when the tool came from a toolset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolset_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControlEphemeral>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `{"type": "tool_result", "tool_use_id": …, "content": …}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultBlock {
    /// Constant `"tool_result"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub tool_use_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<ToolResultContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControlEphemeral>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `tool_result.content`: a bare string or nested content blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<AnthropicContentBlock>),
}

/// `{"type": "thinking", "thinking": …, "signature": …}`.
///
/// Must be replayed back to the model verbatim and in order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingBlock {
    /// Constant `"thinking"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub thinking: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `{"type": "redacted_thinking", "data": …}` — encrypted thinking history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedactedThinkingBlock {
    /// Constant `"redacted_thinking"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub data: String,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `{"type": "server_tool_use", "id": …, "name": …, "input": {…}}` — a tool
/// call issued by Anthropic's servers (web search, code execution, …).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerToolUseBlock {
    /// Constant `"server_tool_use"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub name: String,
    /// Same three-state rule as [`ToolUseBlock::input`]: absent stays absent,
    /// explicit `"input": {}` stays an empty object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller: Option<ToolCaller>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Error object returned instead of a server-tool result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerToolError {
    /// e.g. `"web_search_tool_result_error"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// e.g. `"max_uses_exceeded"`, `"unavailable"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// One `web_search_result` entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSearchResult {
    /// Constant `"web_search_result"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_age: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `{"type": "web_search_tool_result", "tool_use_id": …, "content": …}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSearchToolResultBlock {
    /// Constant `"web_search_tool_result"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub tool_use_id: String,
    pub content: WebSearchToolResultContent,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Payload of a [`WebSearchToolResultBlock`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WebSearchToolResultContent {
    Results(Vec<WebSearchResult>),
    Error(ServerToolError),
    Unknown(Value),
}

/// `{"type": "web_fetch_tool_result", "tool_use_id": …, "content": …}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebFetchToolResultBlock {
    /// Constant `"web_fetch_tool_result"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub tool_use_id: String,
    pub content: WebFetchToolResultContent,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Payload of a [`WebFetchToolResultBlock`].
//
// DTO unions are short-lived values; boxing the document payload would only
// add deref noise at every codec call site.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WebFetchToolResultContent {
    Result(WebFetchResult),
    Error(ServerToolError),
    Unknown(Value),
}

/// A fetched document (`{"type": "web_fetch_result", …}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebFetchResult {
    /// Constant `"web_fetch_result"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<DocumentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrieved_at: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `{"type": "code_execution_tool_result", "tool_use_id": …, "content": …}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeExecutionToolResultBlock {
    /// Constant `"code_execution_tool_result"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub tool_use_id: String,
    pub content: CodeExecutionToolResultContent,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Payload of a [`CodeExecutionToolResultBlock`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CodeExecutionToolResultContent {
    Result(CodeExecutionResult),
    Error(ServerToolError),
    Unknown(Value),
}

/// Output of a code-execution run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeExecutionResult {
    /// Constant `"code_execution_result"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub stdout: String,
    pub stderr: String,
    pub return_code: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<Value>>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `{"type": "bash_code_execution_tool_result", …}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BashCodeExecutionToolResultBlock {
    /// Constant `"bash_code_execution_tool_result"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub tool_use_id: String,
    pub content: BashCodeExecutionToolResultContent,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Payload of a [`BashCodeExecutionToolResultBlock`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BashCodeExecutionToolResultContent {
    Result(BashCodeExecutionResult),
    Error(ServerToolError),
    Unknown(Value),
}

/// Output of a bash tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BashCodeExecutionResult {
    /// Constant `"bash_code_execution_result"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub stdout: String,
    pub stderr: String,
    pub return_code: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<BashCodeExecutionOutput>>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// One file produced by a bash tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BashCodeExecutionOutput {
    /// Constant `"bash_code_execution_output"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub file_id: String,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `{"type": "text_editor_code_execution_tool_result", …}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextEditorCodeExecutionToolResultBlock {
    /// Constant `"text_editor_code_execution_tool_result"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub tool_use_id: String,
    pub content: TextEditorCodeExecutionToolResultContent,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Payload of a [`TextEditorCodeExecutionToolResultBlock`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TextEditorCodeExecutionToolResultContent {
    Create(TextEditorCreateResult),
    View(TextEditorViewResult),
    StrReplace(TextEditorStrReplaceResult),
    Error(ServerToolError),
    Unknown(Value),
}

/// `text_editor_code_execution_create_result`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextEditorCreateResult {
    #[serde(rename = "type")]
    pub kind: String,
    /// `true` when an existing file was overwritten.
    pub is_file_update: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `text_editor_code_execution_view_result`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextEditorViewResult {
    #[serde(rename = "type")]
    pub kind: String,
    /// `"text"` / `"image"` / …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    pub num_lines: u64,
    pub start_line: u64,
    pub total_lines: u64,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `text_editor_code_execution_str_replace_result`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextEditorStrReplaceResult {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<Vec<String>>,
    pub new_lines: u64,
    pub new_start: u64,
    pub old_lines: u64,
    pub old_start: u64,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `{"type": "tool_search_tool_result", …}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSearchToolResultBlock {
    /// Constant `"tool_search_tool_result"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub tool_use_id: String,
    pub content: ToolSearchToolResultContent,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Payload of a [`ToolSearchToolResultBlock`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolSearchToolResultContent {
    Result(ToolSearchResult),
    Error(ServerToolError),
    Unknown(Value),
}

/// Tools discovered by a deferred tool search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSearchResult {
    /// Constant `"tool_search_tool_result"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub tool_uses: Vec<ToolUseBlock>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `{"type": "container_upload", "file_id": …}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerUploadBlock {
    /// Constant `"container_upload"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub file_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControlEphemeral>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Any content block, request or response side.
///
/// Covers the 15 documented block types; everything else (future block types,
/// non-object payloads) is preserved verbatim in [`Self::Unknown`].
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum AnthropicContentBlock {
    Text(TextBlock),
    Image(ImageBlock),
    Document(DocumentBlock),
    ToolUse(ToolUseBlock),
    ToolResult(ToolResultBlock),
    Thinking(ThinkingBlock),
    RedactedThinking(RedactedThinkingBlock),
    ServerToolUse(ServerToolUseBlock),
    WebSearchToolResult(WebSearchToolResultBlock),
    WebFetchToolResult(WebFetchToolResultBlock),
    CodeExecutionToolResult(CodeExecutionToolResultBlock),
    BashCodeExecutionToolResult(BashCodeExecutionToolResultBlock),
    TextEditorCodeExecutionToolResult(TextEditorCodeExecutionToolResultBlock),
    ToolSearchToolResult(ToolSearchToolResultBlock),
    ContainerUpload(ContainerUploadBlock),
    Unknown(Value),
}

impl<'de> Deserialize<'de> for AnthropicContentBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserialize_tagged(
            deserializer,
            |tag, value| {
                Some(match tag {
                    "text" => parse(value).map(Self::Text),
                    "image" => parse(value).map(Self::Image),
                    "document" => parse(value).map(Self::Document),
                    "tool_use" => parse(value).map(Self::ToolUse),
                    "tool_result" => parse(value).map(Self::ToolResult),
                    "thinking" => parse(value).map(Self::Thinking),
                    "redacted_thinking" => parse(value).map(Self::RedactedThinking),
                    "server_tool_use" => parse(value).map(Self::ServerToolUse),
                    "web_search_tool_result" => parse(value).map(Self::WebSearchToolResult),
                    "web_fetch_tool_result" => parse(value).map(Self::WebFetchToolResult),
                    "code_execution_tool_result" => parse(value).map(Self::CodeExecutionToolResult),
                    "bash_code_execution_tool_result" => {
                        parse(value).map(Self::BashCodeExecutionToolResult)
                    }
                    "text_editor_code_execution_tool_result" => {
                        parse(value).map(Self::TextEditorCodeExecutionToolResult)
                    }
                    "tool_search_tool_result" => parse(value).map(Self::ToolSearchToolResult),
                    "container_upload" => parse(value).map(Self::ContainerUpload),
                    _ => return None,
                })
            },
            Self::Unknown,
        )
    }
}

// ═══════════════════════════════════════════════════════════════════
// Tools
// ═══════════════════════════════════════════════════════════════════

/// A client-defined tool (`tools[]` entry without a `type` discriminator).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    /// Tool name, 1–128 chars.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema of the tool input.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControlEphemeral>,
    /// Enforce strict schema validation on the model's tool input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    /// Which callers may invoke the tool (`"direct"`,
    /// `"code_execution_2025…"`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_callers: Option<Vec<String>>,
    /// Load the tool lazily instead of in the initial prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defer_loading: Option<bool>,
    /// Stream tool input as it is generated (beta).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eager_input_streaming: Option<bool>,
    /// Example inputs used to steer tool use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_examples: Option<Vec<Value>>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// An Anthropic-provided tool (`tools[]` entry carrying a versioned `type`,
/// e.g. `bash_20250124`, `web_search_20260318`, `computer_20250124`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuiltinTool {
    /// Versioned tool identifier, e.g. `"text_editor_20250728"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControlEphemeral>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_uses: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_domains: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_location: Option<UserLocation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub citations: Option<CitationsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_content_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_width_px: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_height_px: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_number: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_characters: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_callers: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defer_loading: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eager_input_streaming: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Approximate user location for server-side web tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserLocation {
    /// Constant `"approximate"`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `tools[]` union: custom tools vs Anthropic-provided (versioned) tools.
///
/// Dispatch is on the presence of `type`, which only built-in tools carry.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ToolUnion {
    Custom(Tool),
    Builtin(BuiltinTool),
    Unknown(Value),
}

impl<'de> Deserialize<'de> for ToolUnion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        if !value.is_object() {
            return Ok(Self::Unknown(value));
        }
        if value.get("type").is_some() {
            parse(&value)
                .map(Self::Builtin)
                .map_err(serde::de::Error::custom)
        } else {
            parse(&value)
                .map(Self::Custom)
                .map_err(serde::de::Error::custom)
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// Tool choice
// ═══════════════════════════════════════════════════════════════════

/// `tool_choice`: `auto` | `any` | `tool` | `none`.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ToolChoice {
    Auto(ToolChoiceAuto),
    Any(ToolChoiceAny),
    Tool(ToolChoiceTool),
    None(ToolChoiceNone),
    Unknown(Value),
}

impl<'de> Deserialize<'de> for ToolChoice {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserialize_tagged(
            deserializer,
            |tag, value| {
                Some(match tag {
                    "auto" => parse(value).map(Self::Auto),
                    "any" => parse(value).map(Self::Any),
                    "tool" => parse(value).map(Self::Tool),
                    "none" => parse(value).map(Self::None),
                    _ => return None,
                })
            },
            Self::Unknown,
        )
    }
}

/// `{"type": "auto"}` — the model decides.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolChoiceAuto {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_parallel_tool_use: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `{"type": "any"}` — the model must call at least one tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolChoiceAny {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_parallel_tool_use: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `{"type": "tool", "name": …}` — force one specific tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolChoiceTool {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_parallel_tool_use: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `{"type": "none"}` — no tool may be called.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolChoiceNone {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(flatten)]
    pub extra: Extra,
}

// ═══════════════════════════════════════════════════════════════════
// Extended thinking
// ═══════════════════════════════════════════════════════════════════

/// `thinking`: `enabled` | `adaptive` | `disabled`.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ThinkingConfig {
    Enabled(ThinkingConfigEnabled),
    Adaptive(ThinkingConfigAdaptive),
    Disabled(ThinkingConfigDisabled),
    Unknown(Value),
}

impl<'de> Deserialize<'de> for ThinkingConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserialize_tagged(
            deserializer,
            |tag, value| {
                Some(match tag {
                    "enabled" => parse(value).map(Self::Enabled),
                    "adaptive" => parse(value).map(Self::Adaptive),
                    "disabled" => parse(value).map(Self::Disabled),
                    _ => return None,
                })
            },
            Self::Unknown,
        )
    }
}

/// `{"type": "enabled", "budget_tokens": …}` — fixed thinking budget.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingConfigEnabled {
    #[serde(rename = "type")]
    pub kind: String,
    /// Must be ≥ 1024 and smaller than `max_tokens`.
    pub budget_tokens: u32,
    /// `"summarized"` (default) or `"omitted"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `{"type": "adaptive"}` — the model picks its own thinking budget.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingConfigAdaptive {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `{"type": "disabled"}` — no extended thinking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingConfigDisabled {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(flatten)]
    pub extra: Extra,
}

// ═══════════════════════════════════════════════════════════════════
// Output config / container
// ═══════════════════════════════════════════════════════════════════

/// `output_config`: structured-output format plus reasoning effort.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<OutputFormat>,
    /// e.g. `"low"` / `"medium"` / `"high"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `output_config.format` (`{"type": "json_schema", "schema": {…}}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputFormat {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Code-execution container reference: a bare id or a parameter object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ContainerRef {
    Id(String),
    Info(ContainerInfo),
}

/// Container parameters / metadata (`id`, `expires_at`, `skills`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<ContainerSkill>>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// One skill loaded into a container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerSkill {
    /// `"anthropic"` or `"custom"`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

// ═══════════════════════════════════════════════════════════════════
// Messages
// ═══════════════════════════════════════════════════════════════════

/// One conversation turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageParam {
    /// `"user"` or `"assistant"`; kept as a `String` (never validated).
    pub role: String,
    pub content: MessageContent,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `messages[].content`: a bare string or content blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<AnthropicContentBlock>),
}

// ═══════════════════════════════════════════════════════════════════
// Request
// ═══════════════════════════════════════════════════════════════════

/// `POST /v1/messages` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicMessagesRequest {
    /// Model identifier, e.g. `claude-sonnet-4-6`.
    pub model: String,
    /// Maximum tokens to generate. Required by Anthropic; optional here so
    /// the gateway can keep its tolerant defaulting (documented default
    /// 1024 with a loss entry) instead of failing ingress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Conversation turns.
    pub messages: Vec<MessageParam>,
    /// System prompt, as a string or as text blocks with cache breakpoints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemPrompt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<AnthropicMetadata>,
    /// Custom stop sequences.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    /// Enable SSE streaming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Extended thinking configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    /// Client-defined and Anthropic-provided tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolUnion>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
    /// Cache breakpoint applied to the last cacheable block of the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControlEphemeral>,
    /// Code-execution container (id or parameters).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<ContainerRef>,
    /// `"auto"` or `"standard_only"`; kept as a `String` (never validated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    /// Geographic region for inference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference_geo: Option<String>,
    /// Deprecated: Anthropic docs mark `temperature` deprecated for models
    /// released after Claude Opus 4.6 (thinking-enabled requests also ignore
    /// it). Kept typed for protocol translation and pre-4.6 models.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Deprecated: see [`Self::temperature`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Deprecated: see [`Self::temperature`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    /// Any other field, forwarded verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

// ═══════════════════════════════════════════════════════════════════
// Response
// ═══════════════════════════════════════════════════════════════════

/// `Message` response object returned by `POST /v1/messages`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicMessagesResponse {
    /// Message id, e.g. `msg_…`.
    pub id: String,
    /// Constant `"message"`; kept as a `String` (never validated).
    #[serde(rename = "type")]
    pub kind: String,
    /// Constant `"assistant"`.
    pub role: String,
    /// Generated content blocks.
    #[serde(default)]
    pub content: Vec<AnthropicContentBlock>,
    /// Model that actually produced the response.
    pub model: String,
    /// `"end_turn"` / `"max_tokens"` / `"stop_sequence"` / `"tool_use"` /
    /// `"refusal"`; kept as a `String` (never validated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// The stop sequence that was hit, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_details: Option<RefusalStopDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<ContainerInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<AnthropicUsage>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Details attached to a `stop_reason: "refusal"` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefusalStopDetails {
    /// Constant `"refusal"`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Anthropic `usage` object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    /// Tokens written to the prompt cache (legacy aggregate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    /// Tokens served from the prompt cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    /// Per-TTL breakdown of cache writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<CacheCreationUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens_details: Option<OutputTokensUsage>,
    /// Server-side tool call counters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<ServerToolUsage>,
    /// `"standard"` / `"priority"` / `"batch"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference_geo: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `usage.cache_creation` — cache writes split by TTL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheCreationUsage {
    #[serde(default)]
    pub ephemeral_5m_input_tokens: u64,
    #[serde(default)]
    pub ephemeral_1h_input_tokens: u64,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `usage.output_tokens_details`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputTokensUsage {
    #[serde(default)]
    pub thinking_tokens: u64,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `usage.server_tool_use`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerToolUsage {
    #[serde(default)]
    pub web_fetch_requests: u64,
    #[serde(default)]
    pub web_search_requests: u64,
    #[serde(flatten)]
    pub extra: Extra,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decode(value: &Value) -> AnthropicMessagesRequest {
        serde_json::from_value(value.clone()).expect("request should deserialize")
    }

    fn full_request() -> Value {
        json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 4096,
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "pondering", "signature": "sig"},
                    {"type": "text", "text": "hello"}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "42"}
                ]}
            ],
            "system": [
                {"type": "text", "text": "be terse", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
            ],
            "metadata": {"user_id": "u-1"},
            "stop_sequences": ["END"],
            "stream": true,
            "thinking": {"type": "enabled", "budget_tokens": 2048, "display": "summarized"},
            "tools": [
                {"name": "get_weather", "description": "weather", "input_schema": {"type": "object"}},
                {"type": "web_search_20260318", "name": "web_search", "max_uses": 5}
            ],
            "tool_choice": {"type": "tool", "name": "get_weather", "disable_parallel_tool_use": true},
            "output_config": {"format": {"type": "json_schema", "schema": {"type": "object"}}, "effort": "high"},
            "cache_control": {"type": "ephemeral"},
            "container": "container_1",
            "service_tier": "auto",
            "inference_geo": "us",
            "temperature": 0.5,
            "top_p": 0.9,
            "top_k": 40
        })
    }

    fn full_response() -> Value {
        json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "pondering", "signature": "sig"},
                {"type": "text", "text": "hello", "citations": [
                    {"type": "char_location", "document_index": 0, "start_char_index": 0,
                     "end_char_index": 3, "cited_text": "hi"}
                ]},
                {"type": "tool_use", "id": "toolu_1", "name": "get_weather",
                 "input": {"city": "SF"}, "caller": {"type": "direct"}}
            ],
            "model": "claude-sonnet-4-6",
            "stop_reason": "tool_use",
            "stop_sequence": null,
            "container": {"id": "container_1", "expires_at": "2026-01-01T00:00:00Z"},
            "usage": {
                "input_tokens": 10,
                "output_tokens": 20,
                "cache_creation_input_tokens": 3,
                "cache_read_input_tokens": 4,
                "cache_creation": {"ephemeral_5m_input_tokens": 1, "ephemeral_1h_input_tokens": 2},
                "output_tokens_details": {"thinking_tokens": 5},
                "server_tool_use": {"web_fetch_requests": 1, "web_search_requests": 2},
                "service_tier": "standard",
                "inference_geo": "us"
            }
        })
    }

    // ── ① request / response roundtrip ──────────────────────────

    #[test]
    fn request_roundtrips_losslessly() {
        let body = full_request();
        let request = decode(&body);
        assert_eq!(request.model, "claude-sonnet-4-6");
        assert_eq!(request.max_tokens, Some(4096));
        assert_eq!(request.temperature, Some(0.5));
        assert_eq!(request.top_p, Some(0.9));
        assert_eq!(request.top_k, Some(40));
        assert!(request.extra.is_empty());
        assert_eq!(serde_json::to_value(&request).unwrap(), body);
    }

    #[test]
    fn response_roundtrips_losslessly() {
        let body = full_response();
        let response: AnthropicMessagesResponse =
            serde_json::from_value(body.clone()).expect("response should deserialize");
        assert_eq!(response.kind, "message");
        assert_eq!(response.stop_reason.as_deref(), Some("tool_use"));
        // `stop_sequence: null` is dropped, every other field survives.
        let mut expected = body.clone();
        expected.as_object_mut().unwrap().remove("stop_sequence");
        assert_eq!(serde_json::to_value(&response).unwrap(), expected);

        let usage = response.usage.unwrap();
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.cache_creation.unwrap().ephemeral_1h_input_tokens, 2);
        assert_eq!(usage.output_tokens_details.unwrap().thinking_tokens, 5);
        assert_eq!(usage.server_tool_use.unwrap().web_search_requests, 2);
    }

    #[test]
    fn response_content_blocks_dispatch_by_type() {
        let response: AnthropicMessagesResponse = serde_json::from_value(full_response()).unwrap();
        assert!(matches!(
            response.content[0],
            AnthropicContentBlock::Thinking(_)
        ));
        assert!(matches!(
            response.content[1],
            AnthropicContentBlock::Text(_)
        ));
        assert!(matches!(
            response.content[2],
            AnthropicContentBlock::ToolUse(_)
        ));
        match &response.content[1] {
            AnthropicContentBlock::Text(block) => {
                let citations = block.citations.as_ref().unwrap();
                assert_eq!(citations[0].kind, "char_location");
                assert_eq!(citations[0].extra.get("cited_text"), Some(&json!("hi")));
            }
            other => panic!("expected text block, got {other:?}"),
        }
    }

    // ── ② Unknown variant fallback ──────────────────────────────

    #[test]
    fn unknown_content_block_is_preserved_verbatim() {
        let body = json!({
            "model": "m",
            "max_tokens": 1,
            "messages": [{"role": "user", "content": [
                {"type": "future_block", "payload": {"nested": [1, 2, 3]}}
            ]}]
        });
        let request = decode(&body);
        let MessageContent::Blocks(blocks) = &request.messages[0].content else {
            panic!("expected blocks");
        };
        match &blocks[0] {
            AnthropicContentBlock::Unknown(value) => {
                assert_eq!(value["type"], json!("future_block"));
                assert_eq!(value["payload"]["nested"], json!([1, 2, 3]));
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
        assert_eq!(serde_json::to_value(&request).unwrap(), body);
    }

    #[test]
    fn non_object_content_block_falls_back_to_unknown() {
        let block: AnthropicContentBlock = serde_json::from_value(json!("just a string")).unwrap();
        assert!(matches!(block, AnthropicContentBlock::Unknown(_)));
        assert_eq!(
            serde_json::to_value(&block).unwrap(),
            json!("just a string")
        );
    }

    #[test]
    fn unknown_union_variants_fall_back_to_unknown() {
        let thinking: ThinkingConfig =
            serde_json::from_value(json!({"type": "future_mode", "budget": 7})).unwrap();
        match thinking {
            ThinkingConfig::Unknown(value) => assert_eq!(value["budget"], json!(7)),
            other => panic!("expected Unknown, got {other:?}"),
        }

        let choice: ToolChoice =
            serde_json::from_value(json!({"type": "future_choice", "x": 1})).unwrap();
        assert!(matches!(choice, ToolChoice::Unknown(_)));

        let source: ImageSource =
            serde_json::from_value(json!({"type": "s3", "bucket": "b"})).unwrap();
        assert!(matches!(source, ImageSource::Unknown(_)));
    }

    // ── ③ extra passthrough ─────────────────────────────────────

    #[test]
    fn unknown_fields_land_in_extra_and_survive_reserialize() {
        let body = json!({
            "model": "m",
            "max_tokens": 16,
            "messages": [{
                "role": "user",
                "content": [{"type": "text", "text": "hi", "future_field": {"a": 1}}],
                "future_role_hint": true
            }],
            "future_top_level": [1, 2]
        });
        let request = decode(&body);
        assert_eq!(request.extra.get("future_top_level"), Some(&json!([1, 2])));
        assert_eq!(
            request.messages[0].extra.get("future_role_hint"),
            Some(&json!(true))
        );
        let MessageContent::Blocks(blocks) = &request.messages[0].content else {
            panic!("expected blocks");
        };
        let AnthropicContentBlock::Text(text) = &blocks[0] else {
            panic!("expected text block");
        };
        assert_eq!(text.extra.get("future_field"), Some(&json!({"a": 1})));
        assert_eq!(serde_json::to_value(&request).unwrap(), body);
    }

    // ── ④ thinking 三变体 ───────────────────────────────────────

    #[test]
    fn thinking_enabled_variant() {
        let body = json!({"type": "enabled", "budget_tokens": 2048, "display": "omitted"});
        let thinking: ThinkingConfig = serde_json::from_value(body.clone()).unwrap();
        match &thinking {
            ThinkingConfig::Enabled(enabled) => {
                assert_eq!(enabled.kind, "enabled");
                assert_eq!(enabled.budget_tokens, 2048);
                assert_eq!(enabled.display.as_deref(), Some("omitted"));
            }
            other => panic!("expected Enabled, got {other:?}"),
        }
        assert_eq!(serde_json::to_value(&thinking).unwrap(), body);
    }

    #[test]
    fn thinking_adaptive_variant() {
        let body = json!({"type": "adaptive", "display": "summarized"});
        let thinking: ThinkingConfig = serde_json::from_value(body.clone()).unwrap();
        match &thinking {
            ThinkingConfig::Adaptive(adaptive) => {
                assert_eq!(adaptive.kind, "adaptive");
                assert_eq!(adaptive.display.as_deref(), Some("summarized"));
            }
            other => panic!("expected Adaptive, got {other:?}"),
        }
        assert_eq!(serde_json::to_value(&thinking).unwrap(), body);
    }

    #[test]
    fn thinking_disabled_variant() {
        let body = json!({"type": "disabled"});
        let thinking: ThinkingConfig = serde_json::from_value(body.clone()).unwrap();
        match &thinking {
            ThinkingConfig::Disabled(disabled) => assert_eq!(disabled.kind, "disabled"),
            other => panic!("expected Disabled, got {other:?}"),
        }
        assert_eq!(serde_json::to_value(&thinking).unwrap(), body);
    }

    // ── ⑤ system: string vs blocks ──────────────────────────────

    #[test]
    fn system_accepts_string_form() {
        let request = decode(&json!({
            "model": "m",
            "max_tokens": 1,
            "messages": [],
            "system": "be terse"
        }));
        match request.system {
            Some(SystemPrompt::Text(text)) => assert_eq!(text, "be terse"),
            other => panic!("expected text system prompt, got {other:?}"),
        }
    }

    #[test]
    fn system_accepts_block_form() {
        let body = json!({
            "model": "m",
            "max_tokens": 1,
            "messages": [],
            "system": [{"type": "text", "text": "be terse", "cache_control": {"type": "ephemeral"}}]
        });
        let request = decode(&body);
        match &request.system {
            Some(SystemPrompt::Blocks(blocks)) => {
                assert_eq!(blocks[0].text, "be terse");
                assert!(blocks[0].cache_control.is_some());
            }
            other => panic!("expected block system prompt, got {other:?}"),
        }
        assert_eq!(serde_json::to_value(&request).unwrap(), body);
    }

    // ── content block coverage ──────────────────────────────────

    #[test]
    fn every_documented_block_type_dispatches_to_a_typed_variant() {
        let cases = [
            (json!({"type": "text", "text": "t"}), "text"),
            (
                json!({"type": "image", "source": {"type": "url", "url": "https://x/y.png"}}),
                "image",
            ),
            (
                json!({"type": "document", "source": {"type": "file", "file_id": "f"}}),
                "document",
            ),
            (
                json!({"type": "tool_use", "id": "t", "name": "n", "input": {"q": "x"}}),
                "tool_use",
            ),
            (
                json!({"type": "tool_result", "tool_use_id": "t"}),
                "tool_result",
            ),
            (json!({"type": "thinking", "thinking": "t"}), "thinking"),
            (
                json!({"type": "redacted_thinking", "data": "d"}),
                "redacted_thinking",
            ),
            (
                json!({"type": "server_tool_use", "id": "s", "name": "web_search",
                       "input": {"query": "x"}}),
                "server_tool_use",
            ),
            (
                json!({"type": "web_search_tool_result", "tool_use_id": "s", "content": [
                    {"type": "web_search_result", "url": "https://x", "title": "X"}
                ]}),
                "web_search_tool_result",
            ),
            (
                json!({"type": "web_fetch_tool_result", "tool_use_id": "s", "content": {
                    "type": "web_fetch_result", "url": "https://x",
                    "content": {"type": "document", "source": {"type": "url", "url": "https://x"}}
                }}),
                "web_fetch_tool_result",
            ),
            (
                json!({"type": "code_execution_tool_result", "tool_use_id": "s", "content": {
                    "type": "code_execution_result", "stdout": "o", "stderr": "", "return_code": 0
                }}),
                "code_execution_tool_result",
            ),
            (
                json!({"type": "bash_code_execution_tool_result", "tool_use_id": "s", "content": {
                    "type": "bash_code_execution_result", "stdout": "o", "stderr": "", "return_code": 0
                }}),
                "bash_code_execution_tool_result",
            ),
            (
                json!({"type": "text_editor_code_execution_tool_result", "tool_use_id": "s",
                       "content": {"type": "text_editor_code_execution_create_result",
                                   "is_file_update": false}}),
                "text_editor_code_execution_tool_result",
            ),
            (
                json!({"type": "tool_search_tool_result", "tool_use_id": "s", "content": {
                    "type": "tool_search_tool_result",
                    "tool_uses": [{"type": "tool_use", "id": "t", "name": "n", "input": {"q": "x"}}]
                }}),
                "tool_search_tool_result",
            ),
            (
                json!({"type": "container_upload", "file_id": "f"}),
                "container_upload",
            ),
        ];

        for (payload, tag) in cases {
            let block: AnthropicContentBlock = serde_json::from_value(payload.clone())
                .unwrap_or_else(|e| panic!("{tag} should deserialize: {e}"));
            assert!(
                !matches!(block, AnthropicContentBlock::Unknown(_)),
                "{tag} fell back to Unknown"
            );
            assert_eq!(
                serde_json::to_value(&block).unwrap(),
                payload,
                "{tag} did not round-trip"
            );
        }

        // An explicit empty `input` is a distinct wire state from an absent
        // one: it must still dispatch to the typed variant *and* round-trip.
        for (payload, tag) in [
            (
                json!({"type": "tool_use", "id": "t", "name": "n", "input": {}}),
                "tool_use",
            ),
            (
                json!({"type": "server_tool_use", "id": "s", "name": "web_search",
                       "input": {}}),
                "server_tool_use",
            ),
        ] {
            let block: AnthropicContentBlock = serde_json::from_value(payload.clone())
                .unwrap_or_else(|e| panic!("{tag} with empty input should deserialize: {e}"));
            assert!(
                !matches!(block, AnthropicContentBlock::Unknown(_)),
                "{tag} with explicit empty input fell back to Unknown"
            );
            assert_eq!(
                serde_json::to_value(&block).unwrap(),
                payload,
                "{tag} with explicit empty input did not round-trip"
            );
        }
    }

    /// `input` is an `Option`, so both wire states round-trip unchanged: a
    /// block that never carried `input` must not grow `"input": {}`, and a
    /// block that *did* send `"input": {}` must keep it. Collapsing the two
    /// would make an explicit empty object indistinguishable from an absent
    /// one.
    #[test]
    fn missing_or_empty_tool_input_is_not_fabricated_on_serialize() {
        for (payload, tag) in [
            (
                json!({"type": "tool_use", "id": "t", "name": "n"}),
                "tool_use",
            ),
            (
                json!({"type": "server_tool_use", "id": "s", "name": "web_search"}),
                "server_tool_use",
            ),
        ] {
            // Absent upstream → absent on the way out.
            let block: AnthropicContentBlock = serde_json::from_value(payload.clone())
                .unwrap_or_else(|e| panic!("{tag} should deserialize: {e}"));
            assert_eq!(
                serde_json::to_value(&block).unwrap(),
                payload,
                "{tag} without input grew an `input` key"
            );

            // Explicit `"input": {}` → verbatim empty object on the way out.
            let mut with_empty_input = payload.clone();
            with_empty_input["input"] = json!({});
            let block: AnthropicContentBlock = serde_json::from_value(with_empty_input.clone())
                .unwrap_or_else(|e| panic!("{tag} with empty input should deserialize: {e}"));
            assert_eq!(
                serde_json::to_value(&block).unwrap(),
                with_empty_input,
                "{tag} with explicit empty input must keep `input`: {{}}"
            );
        }
    }

    #[test]
    fn image_and_document_source_variants() {
        let image: ImageBlock = serde_json::from_value(json!({
            "type": "image",
            "source": {"type": "base64", "media_type": "image/png", "data": "AAA"},
            "transformations": {"rotate": 90}
        }))
        .unwrap();
        assert!(matches!(image.source, ImageSource::Base64(_)));
        assert_eq!(image.transformations, Some(json!({"rotate": 90})));

        let sources = [
            json!({"type": "base64", "media_type": "application/pdf", "data": "AAA"}),
            json!({"type": "text", "media_type": "text/plain", "data": "hello"}),
            json!({"type": "content", "content": [{"type": "text", "text": "hello"}]}),
            json!({"type": "url", "url": "https://x/doc.pdf"}),
            json!({"type": "file", "file_id": "file_1"}),
        ];
        for source in sources {
            let document: DocumentBlock = serde_json::from_value(json!({
                "type": "document",
                "source": source,
                "title": "T"
            }))
            .unwrap_or_else(|e| panic!("source {source} should deserialize: {e}"));
            assert!(!matches!(document.source, DocumentSource::Unknown(_)));
            let value = serde_json::to_value(&document).unwrap();
            assert_eq!(value["source"], source);
        }
    }

    #[test]
    fn tool_result_content_accepts_string_and_blocks() {
        let block: ToolResultBlock = serde_json::from_value(json!({
            "type": "tool_result", "tool_use_id": "t", "content": "plain"
        }))
        .unwrap();
        assert!(matches!(block.content, Some(ToolResultContent::Text(_))));

        let block: ToolResultBlock = serde_json::from_value(json!({
            "type": "tool_result", "tool_use_id": "t",
            "content": [{"type": "text", "text": "nested"}], "is_error": true
        }))
        .unwrap();
        assert!(matches!(block.content, Some(ToolResultContent::Blocks(_))));
        assert_eq!(block.is_error, Some(true));
    }

    // ── tools / tool_choice ─────────────────────────────────────

    #[test]
    fn tool_union_separates_custom_and_builtin_tools() {
        let request = decode(&json!({
            "model": "m",
            "max_tokens": 1,
            "messages": [],
            "tools": [
                {"name": "f", "description": "d", "input_schema": {"type": "object"},
                 "strict": true, "allowed_callers": ["direct"], "defer_loading": true,
                 "eager_input_streaming": false, "input_examples": [{"a": 1}]},
                {"type": "bash_20250124", "name": "bash"},
                {"type": "web_search_20260318", "name": "web_search", "max_uses": 3,
                 "allowed_domains": ["example.com"],
                 "user_location": {"type": "approximate", "city": "SF"}},
                {"type": "future_tool_2027", "name": "future", "brand_new": true}
            ]
        }));
        let tools = request.tools.as_ref().unwrap();
        match &tools[0] {
            ToolUnion::Custom(tool) => {
                assert_eq!(tool.name, "f");
                assert_eq!(tool.strict, Some(true));
                assert_eq!(tool.eager_input_streaming, Some(false));
                assert!(tool.extra.is_empty());
            }
            other => panic!("expected custom tool, got {other:?}"),
        }
        assert!(matches!(&tools[1], ToolUnion::Builtin(tool) if tool.kind == "bash_20250124"));
        match &tools[2] {
            ToolUnion::Builtin(tool) => {
                assert_eq!(tool.max_uses, Some(3));
                assert_eq!(
                    tool.user_location.as_ref().unwrap().city.as_deref(),
                    Some("SF")
                );
            }
            other => panic!("expected builtin tool, got {other:?}"),
        }
        // Unknown versioned tools still parse as builtins (extra keeps the rest).
        match &tools[3] {
            ToolUnion::Builtin(tool) => {
                assert_eq!(tool.kind, "future_tool_2027");
                assert_eq!(tool.extra.get("brand_new"), Some(&json!(true)));
            }
            other => panic!("expected builtin tool, got {other:?}"),
        }
        assert_eq!(
            serde_json::to_value(&request).unwrap()["tools"],
            json!([
                {"name": "f", "description": "d", "input_schema": {"type": "object"},
                 "strict": true, "allowed_callers": ["direct"], "defer_loading": true,
                 "eager_input_streaming": false, "input_examples": [{"a": 1}]},
                {"type": "bash_20250124", "name": "bash"},
                {"type": "web_search_20260318", "name": "web_search", "max_uses": 3,
                 "allowed_domains": ["example.com"],
                 "user_location": {"type": "approximate", "city": "SF"}},
                {"type": "future_tool_2027", "name": "future", "brand_new": true}
            ])
        );
    }

    #[test]
    fn tool_choice_four_variants() {
        let cases = [
            json!({"type": "auto"}),
            json!({"type": "any", "disable_parallel_tool_use": true}),
            json!({"type": "tool", "name": "f"}),
            json!({"type": "none"}),
        ];
        let choice: ToolChoice = serde_json::from_value(cases[0].clone()).unwrap();
        assert!(matches!(choice, ToolChoice::Auto(_)));
        let choice: ToolChoice = serde_json::from_value(cases[1].clone()).unwrap();
        assert!(matches!(choice, ToolChoice::Any(_)));
        let choice: ToolChoice = serde_json::from_value(cases[2].clone()).unwrap();
        assert!(matches!(choice, ToolChoice::Tool(_)));
        let choice: ToolChoice = serde_json::from_value(cases[3].clone()).unwrap();
        assert!(matches!(choice, ToolChoice::None(_)));

        for case in cases {
            let choice: ToolChoice = serde_json::from_value(case.clone()).unwrap();
            assert_eq!(serde_json::to_value(&choice).unwrap(), case);
        }
    }

    // ── container / output config ───────────────────────────────

    #[test]
    fn container_accepts_id_and_object_forms() {
        let request = decode(&json!({
            "model": "m",
            "max_tokens": 1,
            "messages": [],
            "container": {"id": "c1", "skills": [{"type": "anthropic", "skill_id": "s1"}]}
        }));
        match request.container {
            Some(ContainerRef::Info(info)) => {
                assert_eq!(info.id.as_deref(), Some("c1"));
                assert_eq!(info.skills.unwrap()[0].skill_id.as_deref(), Some("s1"));
            }
            other => panic!("expected container info, got {other:?}"),
        }
    }

    #[test]
    fn output_config_format_is_typed() {
        let config: OutputConfig = serde_json::from_value(json!({
            "format": {"type": "json_schema", "schema": {"type": "object"}},
            "effort": "medium"
        }))
        .unwrap();
        assert_eq!(config.format.unwrap().kind, "json_schema");
        assert_eq!(config.effort.as_deref(), Some("medium"));
    }

    #[test]
    fn request_without_optional_fields_keeps_extras_empty() {
        let request = decode(&json!({"model": "m", "messages": []}));
        assert!(request.max_tokens.is_none());
        assert!(request.system.is_none());
        assert!(request.thinking.is_none());
        assert!(request.extra.is_empty());
        // Only the provided fields are re-emitted.
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            json!({"model": "m", "messages": []})
        );
    }
}

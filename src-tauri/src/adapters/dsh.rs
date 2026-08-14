//! Adapter for DeepSeek Harness (`dsh`) — the open-source agent harness from
//! DeepSeek AI (https://github.com/deepseek-ai/deepseek-harness).
//!
//! The harness can be installed two ways, and both are supported here:
//!
//! 1. **npm** — `npx @deepseek-ai/dsh web` (or a global `dsh` install). The
//!    binary lands on `$PATH` and works from any directory.
//! 2. **from source** — a checkout of the repository built with
//!    `pnpm install && pnpm run build`, then run as `pnpm dsh web` from inside
//!    that checkout (the workspace `node_modules/.bin/dsh` shim, which is not
//!    on `$PATH`).
//!
//! Crucially, **both installations persist session data to the same place**:
//! the harness home (`$DSH_HOME` or `~/.dsh`) resolved by
//! `@deepseek-ai/dsh-home-paths`, with sessions under
//! `<home>/sessions/<projectKey>/<encodedSessionId>/session.jsonl[.zstd]`
//! (wired by the base bundle: `session-persistence-jsonl` with
//! `root: dshHomePath('sessions')`). So scanning/parsing is install-agnostic.
//!
//! The adapter only needs to be "smart" about the install when producing a
//! resume command: it prefers a `dsh` binary on `$PATH` (npm/global), falls
//! back to a discovered local checkout (source build), and finally to a
//! one-shot `npx @deepseek-ai/dsh` invocation so an npm-less machine with only
//! a checkout still works.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use super::{AgentAdapter, PlatformPaths, SessionLocation};
use crate::models::*;

/// Directory name of the harness home under the OS home (mirrors
/// `DSH_HOME_DIR_NAME` from `@deepseek-ai/dsh-home-paths`).
const DSH_HOME_DIR_NAME: &str = ".dsh";
/// Environment variable overriding the harness home.
const DSH_HOME_ENV: &str = "DSH_HOME";
/// Zstandard frame magic bytes, little-endian for `0xFD2FB528`.
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// Well-known harness checkout directory names, probed (boundedly) when no
/// `dsh` binary is on `$PATH`, so a source install can still be resumed.
const CHECKOUT_DIR_NAMES: &[&str] = &[
    "deepseek-harness",
    "DeepSeek-Harness",
    "deepseek_harness",
    "dsh",
    "dsh-harness",
];

/// Maximum directory depth for the checkout-probe walk under the home dir.
const CHECKOUT_PROBE_MAX_DEPTH: usize = 5;

/// Directory names never descended into during the checkout-probe walk.
const CHECKOUT_PROBE_SKIP: &[&str] = &[
    "node_modules",
    ".git",
    "Library",
    ".cache",
    ".Trash",
    "AppData",
    "System Volume Information",
];

/// Tools whose arguments name a file the agent touched; used for file_touches.
fn dsh_file_operation_for(tool_name: &str) -> Option<&'static str> {
    let normalized = tool_name
        .rsplit(['/', ':'])
        .next()
        .unwrap_or(tool_name)
        .to_ascii_lowercase();
    match normalized.as_str() {
        "read" | "read_image" => Some("read"),
        "edit" | "str_replace_editor" | "str-replace-editor" => Some("edit"),
        "write" => Some("write"),
        "delete" => Some("delete"),
        "bash" | "pwsh" => None,
        _ => Some("unknown"),
    }
}

/// Extract a file path from a tool-call arguments object, matching the
/// conventions of the harness's own tools (`file_path`, `filePath`, `path`).
fn dsh_extract_file_path(input: &Value) -> Option<String> {
    for key in ["file_path", "filePath", "path"] {
        if let Some(value) = input.get(key).and_then(|v| v.as_str()) {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

pub struct DshAdapter;

impl Default for DshAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl DshAdapter {
    pub fn new() -> Self {
        Self
    }

    // --- install / data-root discovery ------------------------------------

    /// The harness sessions root: `$DSH_HOME/sessions`, else `~/.dsh/sessions`.
    /// Identical for npm and source installs.
    fn sessions_root() -> Option<PathBuf> {
        if let Ok(custom) = std::env::var(DSH_HOME_ENV) {
            let trimmed = custom.trim();
            if !trimmed.is_empty() {
                return Some(PathBuf::from(trimmed).join("sessions"));
            }
        }
        if cfg!(target_os = "windows") {
            Self::windows_sessions_root(&PlatformPaths::system())
        } else {
            dirs::home_dir().map(|home| home.join(DSH_HOME_DIR_NAME).join("sessions"))
        }
    }

    pub(crate) fn windows_sessions_root(paths: &PlatformPaths) -> Option<PathBuf> {
        paths.home_join(DSH_HOME_DIR_NAME).map(|home| home.join("sessions"))
    }

    /// Whether a `dsh` executable is resolvable through `$PATH` (npm/global
    /// install, or a source build whose `node_modules/.bin` is on `$PATH`).
    fn dsh_binary_on_path() -> bool {
        let probe = if cfg!(target_os = "windows") {
            std::process::Command::new("where")
                .arg("dsh")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
        } else {
            std::process::Command::new("which")
                .arg("dsh")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
        };
        matches!(probe, Ok(status) if status.success())
    }

    /// A bounded probe for a source checkout of the harness: well-known
    /// directory names directly under the home directory, plus a depth-capped
    /// walk of the home so a checkout nested under e.g. `~/dev/` or
    /// `~/projects/` is still found. Never walks the whole filesystem, never
    /// descends into huge/system directories.
    fn find_local_checkout() -> Option<PathBuf> {
        let home = dirs::home_dir()?;
        if let Some(found) = Self::probe_known_checkout_locations() {
            return Some(found);
        }
        Self::find_checkout_in(&home, CHECKOUT_PROBE_MAX_DEPTH)
    }

    /// Depth-capped walk of `root` looking for a built checkout. Skips hidden
    /// directories and the huge/system names in `CHECKOUT_PROBE_SKIP`.
    fn find_checkout_in(root: &Path, max_depth: usize) -> Option<PathBuf> {
        let mut stack = vec![(root.to_path_buf(), 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            if depth >= max_depth {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if name.starts_with('.') || CHECKOUT_PROBE_SKIP.contains(&name) {
                    continue;
                }
                if Self::looks_like_built_checkout(&path) {
                    return Some(path);
                }
                stack.push((path, depth + 1));
            }
        }
        None
    }

    /// Probe only well-known checkout names under the home directory (cheap —
    /// safe to run from `detect`). Returns the checkout root when found.
    fn probe_known_checkout_locations() -> Option<PathBuf> {
        let home = dirs::home_dir()?;
        for name in CHECKOUT_DIR_NAMES {
            let candidate = home.join(name);
            if Self::looks_like_built_checkout(&candidate) {
                return Some(candidate);
            }
        }
        if Self::looks_like_built_checkout(&home) {
            return Some(home);
        }
        None
    }

    /// A checkout counts when its CLI bundle exists (npm/source both compile
    /// `apps/cli` → `apps/cli/lib/bin.js` via the `build` script) or a
    /// workspace `dsh` shim exists in `node_modules/.bin`.
    fn looks_like_built_checkout(root: &Path) -> bool {
        root.join("apps").join("cli").join("lib").join("bin.js").is_file()
            || root
                .join("node_modules")
                .join(".bin")
                .join(if cfg!(target_os = "windows") { "dsh.cmd" } else { "dsh" })
                .is_file()
    }

    // --- scanning ----------------------------------------------------------

    fn modified_at(path: &Path) -> DateTime<Utc> {
        std::fs::metadata(path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| {
                DateTime::from_timestamp(
                    t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64,
                    0,
                )
            })
            .unwrap_or_default()
    }

    /// Recursively collect session artifact files. A session log is named
    /// `session.jsonl` (plaintext) or `session.jsonl.zstd` (default
    /// compression) and sits in a session directory under a project-key
    /// directory. Project keys are encoded (`--Users-foo-bar--`) and session
    /// ids are escaped, so we match on the artifact basename only and skip
    /// nothing else — no decoding needed for a scan.
    pub(crate) fn scan_root(root: &Path) -> Vec<SessionLocation> {
        let mut locations = Vec::new();
        Self::scan_dir(root, &mut locations);
        locations
    }

    fn scan_dir(dir: &Path, locations: &mut Vec<SessionLocation>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                Self::scan_dir(&path, locations);
            } else if Self::is_session_artifact(&path) {
                locations.push(SessionLocation {
                    last_modified: Self::modified_at(&path),
                    path,
                });
            }
        }
    }

    fn is_session_artifact(path: &Path) -> bool {
        path.file_name()
            .and_then(|name| name.to_str())
            .map(|name| name == "session.jsonl" || name == "session.jsonl.zstd")
            .unwrap_or(false)
    }

    // --- log reading -------------------------------------------------------

    /// Read a session artifact to text, decompressing Zstandard logs (with
    /// tolerance for a torn final frame from a crash-orphaned session: complete
    /// frames are kept, the incomplete tail is dropped, mirroring the harness's
    /// own repair-on-read behavior).
    fn read_log(path: &Path) -> Result<String, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
        let is_zstd = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e == "zstd")
            .unwrap_or(false)
            || bytes.starts_with(&ZSTD_MAGIC);
        if is_zstd {
            Self::decompress_zstd(&bytes)
                .map_err(|e| format!("Failed to decompress {}: {}", path.display(), e))
        } else {
            String::from_utf8(bytes)
                .map_err(|e| format!("Session log {} is not valid UTF-8: {}", path.display(), e))
        }
    }

    /// Decompress a concatenated Zstandard frame stream. The streaming decoder
    /// resets at frame boundaries; a checksum failure or torn final frame
    /// surfaces as a read error, at which point complete frames already
    /// yielded are preserved and scanning stops.
    fn decompress_zstd(bytes: &[u8]) -> Result<String, String> {
        let mut decoder = zstd::stream::read::Decoder::new(bytes)
            .map_err(|e| format!("invalid zstd stream: {}", e))?;
        let mut out = Vec::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            match decoder.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buffer[..n]),
                Err(_) => break, // torn tail — keep the valid prefix
            }
        }
        String::from_utf8(out).map_err(|e| format!("decoded bytes are not valid UTF-8: {}", e))
    }

    // --- parsing -----------------------------------------------------------

    fn json_str<'a>(json: &'a Value, path: &[&str]) -> Option<&'a str> {
        let mut current = json;
        for key in path {
            current = current.get(*key)?;
        }
        current.as_str()
    }

    fn timestamp_from_millis(value: Option<i64>) -> Option<DateTime<Utc>> {
        DateTime::from_timestamp_millis(value?)
    }

    /// Extract plain-text content from a message `content` block array,
    /// joining `text` blocks (reasoning blocks are intentionally excluded from
    /// the transcript body, matching the harness's own UI split).
    fn text_from_blocks(content: &Value) -> String {
        let Some(blocks) = content.as_array() else {
            if let Some(text) = content.as_str() {
                return text.to_string();
            }
            return String::new();
        };
        let mut parts = Vec::new();
        for block in blocks {
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    if !text.is_empty() {
                        parts.push(text.to_string());
                    }
                }
            }
        }
        parts.join("\n\n")
    }

    /// Extract the model-facing text of a `tool-result` block's nested content.
    fn text_from_tool_result(message: &Value) -> String {
        let Some(blocks) = message.get("content").and_then(|c| c.as_array()) else {
            return String::new();
        };
        let mut parts = Vec::new();
        for block in blocks {
            if block.get("type").and_then(|t| t.as_str()) == Some("tool-result") {
                let nested = Self::text_from_blocks(block.get("content").unwrap_or(&Value::Null));
                if !nested.is_empty() {
                    parts.push(nested);
                }
            }
        }
        parts.join("\n\n")
    }

    /// Sum a usage field across assistant messages; missing/absent → 0.
    fn usage_field(usage: &Value, key: &str) -> u64 {
        usage.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
    }

    pub(crate) fn parse_log_text(
        text: &str,
        path: &Path,
    ) -> Result<NormalizedSession, String> {
        let mut lines = text.lines();
        let header_line = lines.next().ok_or_else(|| "empty session log".to_string())?;
        let header: Value = serde_json::from_str(header_line)
            .map_err(|e| format!("corrupt session log header: {}", e))?;
        if header.get("type").and_then(|t| t.as_str()) != Some("session") {
            return Err("session log does not start with a session header".to_string());
        }

        let session_id = Self::json_str(&header, &["id"]).map(ToString::to_string).unwrap_or_else(|| {
            path.file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        });
        let created_at = Self::timestamp_from_millis(
            header.get("createdAt").and_then(|v| v.as_i64()),
        )
        .unwrap_or_else(Utc::now);
        let parent_session_id = Self::json_str(&header, &["parentSession"]).map(ToString::to_string);
        let project_path = Self::json_str(&header, &["cwd"]).unwrap_or_default().to_string();

        let mut messages: Vec<Message> = Vec::new();
        let mut file_touches: Vec<FileTouch> = Vec::new();
        let mut title: Option<String> = None;
        let mut model: Option<String> = None;
        let mut updated_at = created_at;
        let mut total_input: u64 = 0;
        let mut total_output: u64 = 0;
        let mut total_cached: u64 = 0;
        let mut total_reasoning: u64 = 0;

        // callId -> (tool_name, tool_input); tool results pair with their call.
        let mut pending_tool_calls: HashMap<String, (String, String)> = HashMap::new();
        // Positions of Tool messages created from a call, by callId, so a
        // result can fill in tool_output on the same row.
        let mut tool_message_index: HashMap<String, usize> = HashMap::new();

        for line in lines {
            if line.trim().is_empty() {
                continue;
            }
            let record: Value = match serde_json::from_str(line) {
                Ok(value) => value,
                Err(_) => continue,
            };
            let Some(event_type) = Self::json_str(&record, &["type"]) else {
                continue;
            };
            // Packed chunk runs (default `packChunks: true`) carry token
            // deltas only; assembled content arrives in `assistant/message`.
            if matches!(
                event_type,
                "text-chunks" | "reasoning-chunks" | "tool-call-chunks"
            ) {
                continue;
            }
            let time = Self::timestamp_from_millis(record.get("time").and_then(|t| t.as_i64()))
                .unwrap_or_else(Utc::now);
            let seq = messages.len() as u32;
            let data = record.get("data").cloned().unwrap_or(Value::Null);

            match event_type {
                "user/message" => {
                    let source_kind = Self::json_str(&data, &["source", "kind"]).unwrap_or("user");
                    // The harness injects scaffolding (runtime-context
                    // snapshots, AGENTS.md preambles, skill content) as
                    // user-role messages with `source.kind: plugin`; those are
                    // reclassified to Orbit's Context role, while `kind: user`
                    // is a genuine prompt.
                    let role = match source_kind {
                        "user" => MessageRole::User,
                        "plugin" => MessageRole::Context,
                        _ => MessageRole::Context,
                    };
                    let content = Self::text_from_blocks(data.get("content").unwrap_or(&Value::Null));
                    if title.is_none() && role == MessageRole::User && !content.trim().is_empty() {
                        title = Some(content.chars().take(100).collect());
                    }
                    messages.push(Message {
                        id: uuid::Uuid::new_v4().to_string(),
                        session_id: session_id.clone(),
                        role,
                        content,
                        timestamp: Some(time),
                        sequence: seq,
                        tool_name: None,
                        tool_input: None,
                        tool_output: None,
                    });
                }
                "assistant/message" => {
                    let message = data.get("message").unwrap_or(&Value::Null);
                    let content = Self::text_from_blocks(
                        message.get("content").unwrap_or(&Value::Null),
                    );
                    if model.is_none() {
                        model = Self::json_str(message, &["source", "model"])
                            .map(ToString::to_string);
                    }
                    if let Some(usage) = data.get("usage") {
                        total_input += Self::usage_field(usage, "inputTokens");
                        total_output += Self::usage_field(usage, "outputTokens");
                        total_cached += Self::usage_field(usage, "cacheReadTokens")
                            + Self::usage_field(usage, "cacheWriteTokens");
                        total_reasoning += Self::usage_field(usage, "reasoningTokens");
                    }
                    messages.push(Message {
                        id: uuid::Uuid::new_v4().to_string(),
                        session_id: session_id.clone(),
                        role: MessageRole::Assistant,
                        content,
                        timestamp: Some(time),
                        sequence: seq,
                        tool_name: None,
                        tool_input: None,
                        tool_output: None,
                    });
                }
                "tool/call" => {
                    let call_id = Self::json_str(&data, &["callId"])
                        .unwrap_or_default()
                        .to_string();
                    let name = Self::json_str(&data, &["name"]).unwrap_or_default().to_string();
                    let arguments = Self::json_str(&data, &["arguments"])
                        .unwrap_or_default()
                        .to_string();
                    pending_tool_calls.insert(call_id.clone(), (name.clone(), arguments.clone()));

                    if let Ok(parsed_args) = serde_json::from_str::<Value>(&arguments) {
                        if let Some(path) = dsh_extract_file_path(&parsed_args) {
                            let operation = dsh_file_operation_for(&name)
                                .unwrap_or("unknown")
                                .to_string();
                            file_touches.push(FileTouch {
                                path,
                                operation,
                                sequence: seq,
                            });
                        }
                    }

                    tool_message_index.insert(
                        call_id,
                        messages.len(),
                    );
                    messages.push(Message {
                        id: uuid::Uuid::new_v4().to_string(),
                        session_id: session_id.clone(),
                        role: MessageRole::Tool,
                        content: String::new(),
                        timestamp: Some(time),
                        sequence: seq,
                        tool_name: Some(name),
                        tool_input: Some(arguments),
                        tool_output: None,
                    });
                }
                "tool/result" => {
                    let message = data.get("message").unwrap_or(&Value::Null);
                    let call_id = Self::json_str(message, &["source", "callId"])
                        .unwrap_or_default()
                        .to_string();
                    let output = Self::text_from_tool_result(message);
                    if let Some(&index) = tool_message_index.get(&call_id) {
                        if let Some(target) = messages.get_mut(index) {
                            target.tool_output = Some(output);
                        }
                    } else {
                        // Orphaned result (e.g. log truncated before its call).
                        let name = pending_tool_calls
                            .get(&call_id)
                            .map(|(name, _)| name.clone())
                            .unwrap_or_else(|| "tool".to_string());
                        messages.push(Message {
                            id: uuid::Uuid::new_v4().to_string(),
                            session_id: session_id.clone(),
                            role: MessageRole::Tool,
                            content: String::new(),
                            timestamp: Some(time),
                            sequence: seq,
                            tool_name: Some(name),
                            tool_input: None,
                            tool_output: Some(output),
                        });
                    }
                }
                "session/title" => {
                    let value = Self::json_str(&data, &["title"])
                        .filter(|t| !t.trim().is_empty())
                        .map(ToString::to_string);
                    if value.is_some() {
                        title = value;
                    }
                }
                "request/header" => {
                    if model.is_none() {
                        model = Self::json_str(&data, &["header", "config", "model"])
                            .map(ToString::to_string);
                    }
                }
                _ => {}
            }

            if time > updated_at {
                updated_at = time;
            }
        }

        let title = title
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| "Untitled".to_string());

        let session = Session {
            id: session_id,
            parent_session_id,
            agent: AgentType::Dsh,
            title,
            project_path,
            created_at,
            updated_at,
            file_path: path.to_string_lossy().to_string(),
            is_active: false,
            message_count: messages.len() as u32,
            model,
            git_branch: None,
            input_tokens: total_input,
            output_tokens: total_output,
            cached_tokens: total_cached,
            reasoning_tokens: total_reasoning,
            file_count: file_touches.len() as u32,
        };

        Ok(NormalizedSession {
            session,
            messages,
            attachments: Vec::new(),
            file_touches,
        })
    }

    // --- resume command resolution ----------------------------------------

    /// Pure command builder so every installation branch is unit-testable.
    /// `binary_on_path` = a `dsh` executable resolves on `$PATH` (npm/global
    /// install); `checkout` = a discovered built source checkout.
    pub(crate) fn unix_resume_command_for(
        project_path: &str,
        binary_on_path: bool,
        checkout: Option<&Path>,
    ) -> String {
        let safe_path = crate::shell_quote::shell_quote(project_path);
        if binary_on_path {
            format!("cd {} && dsh web", safe_path)
        } else if let Some(checkout) = checkout {
            let safe_checkout = crate::shell_quote::shell_quote(&checkout.to_string_lossy());
            format!("cd {} && pnpm dsh web", safe_checkout)
        } else {
            format!("cd {} && npx --yes @deepseek-ai/dsh web", safe_path)
        }
    }

    pub(crate) fn windows_resume_command_for(
        project_path: &str,
        binary_on_path: bool,
        checkout: Option<&Path>,
    ) -> String {
        let safe_path = crate::shell_quote::shell_quote(project_path);
        if binary_on_path {
            format!("Set-Location {}; dsh web", safe_path)
        } else if let Some(checkout) = checkout {
            let safe_checkout = crate::shell_quote::shell_quote(&checkout.to_string_lossy());
            format!("Set-Location {}; pnpm dsh web", safe_checkout)
        } else {
            format!("Set-Location {}; npx --yes @deepseek-ai/dsh web", safe_path)
        }
    }

    pub(crate) fn windows_resume_command(project_path: &str) -> String {
        let checkout = Self::find_local_checkout();
        Self::windows_resume_command_for(project_path, Self::dsh_binary_on_path(), checkout.as_deref())
    }

    /// Build the shell command that resumes (opens) this harness. `dsh web`
    /// boots the Web UI (default http://127.0.0.1:3080), where the session is
    /// picked from the harness's own sidebar — the harness exposes no
    /// deep-link, and every installation boots the same profile.
    pub(crate) fn unix_resume_command(project_path: &str) -> String {
        let checkout = Self::find_local_checkout();
        Self::unix_resume_command_for(project_path, Self::dsh_binary_on_path(), checkout.as_deref())
    }
}

#[async_trait]
impl AgentAdapter for DshAdapter {
    fn id(&self) -> &str {
        "dsh"
    }

    fn name(&self) -> &str {
        "DeepSeek Harness"
    }

    async fn detect(&self) -> bool {
        // Either the harness has actually stored sessions (both installs write
        // to the same root) or a `dsh` executable / built checkout exists. The
        // checkout probe here is the cheap direct-name one only — the bounded
        // walk is reserved for resume time.
        let root = Self::sessions_root();
        let has_sessions = root
            .as_deref()
            .map(|root| !Self::scan_root(root).is_empty())
            .unwrap_or(false);
        has_sessions
            || Self::dsh_binary_on_path()
            || Self::probe_known_checkout_locations().is_some()
    }

    async fn scan(&self) -> Vec<SessionLocation> {
        match Self::sessions_root() {
            Some(root) if root.is_dir() => Self::scan_root(&root),
            _ => Vec::new(),
        }
    }

    async fn parse_session(&self, path: &Path) -> Result<NormalizedSession, String> {
        let text = Self::read_log(path)?;
        Self::parse_log_text(&text, path)
    }

    fn resume_command(&self, _session_id: &str, project_path: &str) -> String {
        if cfg!(target_os = "windows") {
            return Self::windows_resume_command(project_path);
        }
        Self::unix_resume_command(project_path)
    }

    async fn is_active(&self, _session_path: &Path) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::AgentAdapter;

    fn fixture_log() -> &'static str {
        concat!(
            "{\"type\":\"session\",\"version\":0,\"id\":\"ses_123\",\"createdAt\":1700000000000,\"cwd\":\"/tmp/project\",\"delegationDepth\":0}\n",
            "{\"type\":\"user/message\",\"seq\":0,\"time\":1700000010000,\"data\":{\"content\":[{\"type\":\"text\",\"text\":\"Fix the import\"}],\"source\":{\"kind\":\"user\"},\"role\":\"user\",\"id\":\"m1\"},\"surfaceOp\":\"append\"}\n",
            "{\"type\":\"user/message\",\"seq\":1,\"time\":1700000011000,\"data\":{\"content\":[{\"type\":\"text\",\"text\":\"Current runtime context. This snapshot supersedes earlier runtime-context snapshots.\"}],\"source\":{\"kind\":\"plugin\"},\"role\":\"user\",\"id\":\"m2\"},\"surfaceOp\":\"append\"}\n",
            "{\"type\":\"session/title\",\"seq\":2,\"time\":1700000012000,\"data\":{\"title\":\"Fix the import\",\"messageSeqs\":[0],\"source\":{\"kind\":\"fallback\"}}}\n",
            "{\"type\":\"request/header\",\"seq\":3,\"time\":1700000013000,\"data\":{\"header\":{\"config\":{\"provider\":\"deepseek-official\",\"model\":\"deepseek-v4-flash\"}},\"reason\":\"initial\"}}\n",
            "{\"type\":\"assistant/message\",\"seq\":4,\"time\":1700000020000,\"data\":{\"turn\":1,\"step\":1,\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"reasoning\",\"text\":\"hidden chain\"},{\"type\":\"text\",\"text\":\"I fixed it.\"}],\"source\":{\"kind\":\"model\",\"provider\":\"deepseek-official\",\"model\":\"deepseek-v4-flash\"},\"id\":\"a1\"},\"usage\":{\"inputTokens\":100,\"outputTokens\":50,\"cacheReadTokens\":200,\"reasoningTokens\":30}},\"surfaceOp\":\"append\"}\n",
            "{\"type\":\"tool/call\",\"seq\":5,\"time\":1700000021000,\"data\":{\"turn\":1,\"step\":1,\"callId\":\"call_1\",\"name\":\"tool:read\",\"arguments\":\"{\\\"file_path\\\": \\\"/src/foo.rs\\\"}\"}}\n",
            "{\"type\":\"tool/result\",\"seq\":6,\"time\":1700000022000,\"data\":{\"turn\":1,\"step\":1,\"message\":{\"source\":{\"kind\":\"tool\",\"callId\":\"call_1\"},\"content\":[{\"type\":\"tool-result\",\"toolCallId\":\"call_1\",\"content\":[{\"type\":\"text\",\"text\":\"fn foo() {}\\n\"}],\"isError\":false}],\"role\":\"user\",\"id\":\"t1\"}},\"sourceEventSeqs\":[5],\"surfaceOp\":\"append\"}\n",
            "{\"type\":\"reasoning-chunks\",\"seq0\":7,\"time0\":1700000023000,\"data\":{\"turn\":1,\"step\":1,\"index\":0,\"dt\":[1,1],\"texts\":[\"The\",\" user\",\" says\"]}}\n",
            "{\"type\":\"turn/end\",\"seq\":9,\"time\":1700000025000,\"data\":{\"turn\":1,\"reason\":{\"kind\":\"completed\"}}}\n",
        )
    }

    fn temp_log_path(text: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    #[test]
    fn scans_only_session_artifacts_recursively() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");
        let session_dir = root.join("--tmp-project--").join("ses_123");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("session.jsonl"), "{}").unwrap();
        std::fs::write(session_dir.join("session.jsonl.zstd"), "{}").unwrap();
        std::fs::write(session_dir.join("other.jsonl"), "{}").unwrap();
        std::fs::write(root.join("top-level.jsonl"), "{}").unwrap();

        let locations = DshAdapter::scan_root(&root);
        let mut paths: Vec<_> = locations.into_iter().map(|loc| loc.path).collect();
        paths.sort();

        let mut expected = vec![
            session_dir.join("session.jsonl"),
            session_dir.join("session.jsonl.zstd"),
        ];
        expected.sort();
        assert_eq!(paths, expected);
    }

    #[test]
    fn windows_sessions_root_uses_home() {
        let home = PathBuf::from(r"C:\Users\orbit");
        let paths = PlatformPaths {
            home: Some(home.clone()),
            data: None,
            data_local: None,
        };
        assert_eq!(
            DshAdapter::windows_sessions_root(&paths),
            Some(home.join(".dsh").join("sessions"))
        );
    }

    #[tokio::test]
    async fn parses_plaintext_session_log() {
        let (dir, path) = temp_log_path(fixture_log());
        let adapter = DshAdapter::new();
        let parsed = adapter.parse_session(&path).await.unwrap();

        assert_eq!(parsed.session.id, "ses_123");
        assert_eq!(parsed.session.agent, AgentType::Dsh);
        assert_eq!(parsed.session.title, "Fix the import");
        assert_eq!(parsed.session.project_path, "/tmp/project");
        assert_eq!(parsed.session.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(parsed.session.input_tokens, 100);
        assert_eq!(parsed.session.output_tokens, 50);
        assert_eq!(parsed.session.cached_tokens, 200);
        assert_eq!(parsed.session.reasoning_tokens, 30);
        assert_eq!(parsed.session.message_count, 4);

        // user, context, assistant, tool (call+result paired on one row)
        assert_eq!(parsed.messages[0].role, MessageRole::User);
        assert_eq!(parsed.messages[0].content, "Fix the import");
        assert_eq!(parsed.messages[1].role, MessageRole::Context);
        assert!(parsed.messages[1].content.contains("Current runtime context"));
        assert_eq!(parsed.messages[2].role, MessageRole::Assistant);
        assert_eq!(parsed.messages[2].content, "I fixed it.");
        assert!(!parsed.messages[2].content.contains("hidden chain"));
        assert_eq!(parsed.messages[3].role, MessageRole::Tool);
        assert_eq!(parsed.messages[3].tool_name.as_deref(), Some("tool:read"));
        assert_eq!(
            parsed.messages[3].tool_input.as_deref(),
            Some("{\"file_path\": \"/src/foo.rs\"}")
        );
        assert_eq!(
            parsed.messages[3].tool_output.as_deref(),
            Some("fn foo() {}\n")
        );

        let paths: Vec<&str> = parsed
            .file_touches
            .iter()
            .map(|t| t.path.as_str())
            .collect();
        assert_eq!(paths, vec!["/src/foo.rs"]);
        assert_eq!(parsed.file_touches[0].operation, "read");
        assert_eq!(parsed.session.file_count, 1);
        drop(dir);
    }

    #[tokio::test]
    async fn parses_zstd_compressed_log() {
        let (dir, path) = temp_log_path(fixture_log());
        let compressed_path = dir.path().join("session.jsonl.zstd");
        let encoder = zstd::stream::Encoder::new(Vec::new(), 3).unwrap();
        let mut encoder = encoder;
        std::io::Write::write_all(&mut encoder, fixture_log().as_bytes()).unwrap();
        let compressed = encoder.finish().unwrap();
        std::fs::write(&compressed_path, &compressed).unwrap();
        std::fs::remove_file(&path).unwrap();

        let adapter = DshAdapter::new();
        let parsed = adapter.parse_session(&compressed_path).await.unwrap();
        assert_eq!(parsed.session.id, "ses_123");
        assert_eq!(parsed.session.message_count, 4);
        assert_eq!(parsed.messages[2].content, "I fixed it.");
    }

    #[tokio::test]
    async fn tolerates_torn_zstd_tail() {
        // The harness appends each durable batch as its own Zstandard frame
        // (header batch first, then event batches). Simulate that layout and
        // truncate the final frame's tail, as a crash-orphaned session would.
        let (dir, _) = temp_log_path(fixture_log());
        let compressed_path = dir.path().join("session.jsonl.zstd");
        let log = fixture_log();
        let header_end = log.find('\n').unwrap() + 1;

        let mut frames = Vec::new();
        for (index, segment) in [&log[..header_end], &log[header_end..]].iter().enumerate() {
            let mut encoder = zstd::stream::Encoder::new(Vec::new(), 3).unwrap();
            std::io::Write::write_all(&mut encoder, segment.as_bytes()).unwrap();
            frames.push(encoder.finish().unwrap());
            let _ = index;
        }
        let mut stream = frames[0].clone();
        stream.extend_from_slice(&frames[1]);
        // Drop the last 12 bytes of the final frame.
        let truncated: Vec<u8> = stream[..stream.len() - 12].to_vec();
        std::fs::write(&compressed_path, &truncated).unwrap();

        let adapter = DshAdapter::new();
        let parsed = adapter.parse_session(&compressed_path).await.unwrap();
        assert_eq!(parsed.session.id, "ses_123");
    }

    #[tokio::test]
    async fn falls_back_to_first_user_message_title() {
        let log = concat!(
            "{\"type\":\"session\",\"version\":0,\"id\":\"ses_no_title\",\"createdAt\":1700000000000,\"delegationDepth\":0}\n",
            "{\"type\":\"user/message\",\"seq\":0,\"time\":1700000010000,\"data\":{\"content\":[{\"type\":\"text\",\"text\":\"Hello harness, please help\"}],\"source\":{\"kind\":\"user\"},\"role\":\"user\",\"id\":\"m1\"},\"surfaceOp\":\"append\"}\n",
        );
        let (dir, path) = temp_log_path(log);
        let adapter = DshAdapter::new();
        let parsed = adapter.parse_session(&path).await.unwrap();
        assert_eq!(parsed.session.title, "Hello harness, please help");
        assert_eq!(parsed.session.project_path, "");
        drop(dir);
    }

    #[tokio::test]
    async fn reject_logs_without_session_header() {
        let (dir, path) = temp_log_path("{\"type\":\"turn/start\",\"seq\":0}\n");
        let adapter = DshAdapter::new();
        let result = adapter.parse_session(&path).await;
        assert!(result.is_err());
        drop(dir);
    }

    #[test]
    fn resume_command_covers_every_installation_branch() {
        let checkout = PathBuf::from("/home/dev/deepseek-harness");

        // npm/global install: `dsh` on $PATH.
        let npm = DshAdapter::unix_resume_command_for("/tmp/project", true, None);
        assert_eq!(npm, "cd '/tmp/project' && dsh web");

        // source install: no binary, but a built checkout is found.
        let source =
            DshAdapter::unix_resume_command_for("/tmp/project", false, Some(&checkout));
        assert_eq!(
            source,
            "cd '/home/dev/deepseek-harness' && pnpm dsh web"
        );

        // neither: one-shot npx so a checkout-only machine still resumes.
        let fallback = DshAdapter::unix_resume_command_for("/tmp/project", false, None);
        assert_eq!(fallback, "cd '/tmp/project' && npx --yes @deepseek-ai/dsh web");

        // Windows mirrors each branch with PowerShell syntax. Note: like the
        // other adapters' windows_* helpers, quoting follows the host
        // platform's `shell_quote`, so on Unix the paths stay single-quoted.
        let win_npm = DshAdapter::windows_resume_command_for(r"C:\Work", true, None);
        assert_eq!(win_npm, "Set-Location 'C:\\Work'; dsh web");
        let win_source =
            DshAdapter::windows_resume_command_for(r"C:\Work", false, Some(&checkout));
        assert_eq!(
            win_source,
            "Set-Location '/home/dev/deepseek-harness'; pnpm dsh web"
        );
        let win_fallback = DshAdapter::windows_resume_command_for(r"C:\Work", false, None);
        assert_eq!(
            win_fallback,
            "Set-Location 'C:\\Work'; npx --yes @deepseek-ai/dsh web"
        );
    }

    #[test]
    fn checkout_probe_skips_huge_directories_and_hidden_names() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let mkdir = |rel: &str| std::fs::create_dir_all(home.join(rel)).unwrap();
        // A built checkout inside a huge/system dir must be skipped.
        mkdir("Library/Caches/deepseek-harness/apps/cli/lib");
        std::fs::write(
            home.join("Library/Caches/deepseek-harness/apps/cli/lib/bin.js"),
            "x",
        )
        .unwrap();
        // … inside node_modules …
        mkdir("node_modules/deepseek-harness/apps/cli/lib");
        std::fs::write(
            home.join("node_modules/deepseek-harness/apps/cli/lib/bin.js"),
            "x",
        )
        .unwrap();
        // … inside a hidden dir …
        mkdir(".hidden/deepseek-harness/apps/cli/lib");
        std::fs::write(home.join(".hidden/deepseek-harness/apps/cli/lib/bin.js"), "x").unwrap();
        // … but a real nested checkout is found.
        mkdir("dev/deepseek-harness/apps/cli/lib");
        std::fs::write(home.join("dev/deepseek-harness/apps/cli/lib/bin.js"), "x").unwrap();

        assert_eq!(
            DshAdapter::find_checkout_in(&home, CHECKOUT_PROBE_MAX_DEPTH),
            Some(home.join("dev/deepseek-harness"))
        );
    }
}

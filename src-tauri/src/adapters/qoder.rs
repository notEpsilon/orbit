use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{params, OpenFlags};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::{AgentAdapter, PlatformPaths, SessionLocation};
use crate::models::{AgentType, Message, MessageRole, NormalizedSession, Session};

const QODER_DB_PATH: &str = "Library/Application Support/Qoder/SharedClientCache/cache/db/local.db";

/// Truncates `s` to at most `max_bytes` bytes, respecting UTF-8 char boundaries.
/// Appends "..." if truncation occurred.
fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    // Find the last char boundary at or before max_bytes
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

pub struct QoderAdapter {
    cache: Mutex<Option<QoderDbSnapshot>>,
}

struct QoderDbSnapshot {
    sessions: Vec<QoderSessionRow>,
    messages: HashMap<String, Vec<QoderMessageRow>>,
    /// Plaintext user messages per session (timestamp ms + content), loaded
    /// from the transcript JSONL files under
    /// `~/.qoder/projects/<project>/transcript/`.
    user_messages: HashMap<String, Vec<(i64, String)>>,
}

#[derive(Clone)]
struct QoderSessionRow {
    session_id: String,
    session_title: String,
    project_uri: String,
    gmt_create: i64,
    gmt_modified: i64,
    status: String,
}

#[derive(Clone)]
struct QoderMessageRow {
    _id: String,
    role: String,
    tool_result: Option<String>,
    gmt_create: i64,
    token_info: Option<String>,
}

/// Parses Qoder's `chat_message.token_info` JSON, e.g.
/// `{"prompt_tokens":13777,"completion_tokens":185,"cached_tokens":0,...}`.
/// Returns `(input, output, cached)` tokens. `prompt_tokens` follows the
/// OpenAI convention and includes the cached portion, so the cached share is
/// subtracted to keep Orbit's input/cached buckets separate. Malformed or
/// missing values yield zeros.
fn parse_token_info(token_info: Option<&str>) -> (u64, u64, u64) {
    let Some(raw) = token_info.filter(|s| !s.trim().is_empty()) else {
        return (0, 0, 0);
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return (0, 0, 0);
    };
    let prompt = value
        .get("prompt_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let completion = value
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cached = value
        .get("cached_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    (prompt.saturating_sub(cached), completion, cached)
}

impl QoderAdapter {
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(None),
        }
    }

    pub(crate) fn windows_candidate_db_paths(paths: &PlatformPaths) -> Vec<PathBuf> {
        [
            paths.data_join("Qoder/SharedClientCache/cache/db/local.db"),
            paths.data_local_join("Qoder/SharedClientCache/cache/db/local.db"),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    pub(crate) fn windows_db_path(paths: &PlatformPaths) -> Option<PathBuf> {
        Self::windows_candidate_db_paths(paths)
            .into_iter()
            .find(|path| path.is_file())
    }

    pub(crate) fn windows_resume_command() -> &'static str {
        "Start-Process Qoder"
    }

    fn db_path() -> Option<PathBuf> {
        if cfg!(target_os = "macos") {
            let home = dirs::home_dir()?;
            let path = home.join(QODER_DB_PATH);
            if path.exists() {
                Some(path)
            } else {
                None
            }
        } else if cfg!(target_os = "linux") {
            // To be implemented.
            None
        } else if cfg!(target_os = "windows") {
            Self::windows_db_path(&PlatformPaths::system())
        } else {
            None
        }
    }

    fn load_snapshot(&self) -> Result<QoderDbSnapshot, String> {
        let db_path = Self::db_path().ok_or_else(|| "Qoder DB not found".to_string())?;

        let conn = rusqlite::Connection::open_with_flags(
            &db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| format!("Failed to open Qoder DB: {}", e))?;

        // Load quest sessions
        let mut sess_stmt = conn
            .prepare(
                "SELECT session_id, session_title, project_uri, gmt_create, gmt_modified, status
                 FROM chat_session
                 WHERE session_type = 'quest'
                 ORDER BY gmt_modified DESC",
            )
            .map_err(|e| format!("Failed to prepare session query: {}", e))?;

        let sessions: Vec<QoderSessionRow> = sess_stmt
            .query_map([], |row| {
                Ok(QoderSessionRow {
                    session_id: row.get(0)?,
                    session_title: row.get(1)?,
                    project_uri: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    gmt_create: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    gmt_modified: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    status: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                })
            })
            .map_err(|e| format!("Failed to query sessions: {}", e))?
            .filter_map(|r| r.ok())
            .collect();

        // Load messages for all sessions
        let session_ids: Vec<String> = sessions.iter().map(|s| s.session_id.clone()).collect();
        let mut messages: HashMap<String, Vec<QoderMessageRow>> = HashMap::new();

        if !session_ids.is_empty() {
            let mut msg_stmt = conn
                .prepare(
                    "SELECT id, session_id, role, tool_result, gmt_create, token_info
                     FROM chat_message
                     WHERE session_id = ?1
                     ORDER BY gmt_create ASC",
                )
                .map_err(|e| format!("Failed to prepare message query: {}", e))?;

            for sid in &session_ids {
                let rows: Vec<QoderMessageRow> = msg_stmt
                    .query_map(params![sid], |row| {
                        Ok(QoderMessageRow {
                            _id: row.get(0)?,
                            role: row.get::<_, String>(2)?,
                            tool_result: row.get::<_, Option<String>>(3)?,
                            gmt_create: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                            token_info: row.get::<_, Option<String>>(5)?,
                        })
                    })
                    .map_err(|e| format!("Failed to query messages for {}: {}", sid, e))?
                    .filter_map(|r| r.ok())
                    .collect();
                messages.insert(sid.clone(), rows);
            }
        }

        let user_messages = Self::transcript_projects_root()
            .map(|root| Self::scan_transcript_user_messages(&root))
            .unwrap_or_default();

        Ok(QoderDbSnapshot {
            sessions,
            messages,
            user_messages,
        })
    }

    /// Directory holding Qoder's per-project plaintext transcripts.
    fn transcript_projects_root() -> Option<PathBuf> {
        dirs::home_dir().map(|home| home.join(".qoder").join("projects"))
    }

    /// Scans `<root>/*/transcript/*.jsonl` and collects the plaintext user
    /// messages for each session. The SQLite DB stores user content
    /// encrypted; these transcript files are the readable copy.
    fn scan_transcript_user_messages(root: &Path) -> HashMap<String, Vec<(i64, String)>> {
        let mut map: HashMap<String, Vec<(i64, String)>> = HashMap::new();
        let Ok(project_entries) = std::fs::read_dir(root) else {
            return map;
        };
        for project_entry in project_entries.flatten() {
            let transcript_dir = project_entry.path().join("transcript");
            let Ok(file_entries) = std::fs::read_dir(&transcript_dir) else {
                continue;
            };
            for file_entry in file_entries.flatten() {
                let path = file_entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let Ok(contents) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let mut session_id: Option<String> = None;
                let mut user_msgs = Vec::new();
                for line in contents.lines() {
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                        continue;
                    };
                    if session_id.is_none() {
                        session_id = value
                            .get("sessionId")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                    }
                    if let Some(text) = Self::extract_transcript_user_message(&value) {
                        let ts = value
                            .get("timestamp")
                            .and_then(|v| v.as_str())
                            .and_then(|s| s.parse::<DateTime<Utc>>().ok())
                            .map(|dt| dt.timestamp_millis())
                            .unwrap_or(0);
                        user_msgs.push((ts, text));
                    }
                }
                let sid = session_id
                    .or_else(|| path.file_stem().map(|s| s.to_string_lossy().to_string()));
                if let Some(sid) = sid {
                    if !user_msgs.is_empty() {
                        map.entry(sid).or_default().extend(user_msgs);
                    }
                }
            }
        }
        map
    }

    /// Pops the plaintext user message whose timestamp best matches the DB
    /// row's `gmt_create` (they are written within ~1ms of each other). The
    /// transcript may only cover recent turns of a session, so positional
    /// pairing would misalign; unmatched rows yield `None`.
    fn take_matching_user_message(
        candidates: &mut Vec<(i64, String)>,
        gmt_create: i64,
    ) -> Option<String> {
        const MAX_DIFF_MS: i64 = 5_000;
        let mut best: Option<(usize, i64)> = None;
        for (idx, (ts, _)) in candidates.iter().enumerate() {
            if *ts == 0 {
                continue;
            }
            let diff = (ts - gmt_create).abs();
            if diff <= MAX_DIFF_MS && best.map(|(_, d)| diff < d).unwrap_or(true) {
                best = Some((idx, diff));
            }
        }
        best.map(|(idx, _)| candidates.remove(idx).1)
    }

    /// Extracts the plaintext text of a real user message from a transcript
    /// line. Tool results also arrive under the `user` role but carry
    /// block-array content, so only plain-string content counts.
    fn extract_transcript_user_message(value: &serde_json::Value) -> Option<String> {
        if value.get("type").and_then(|t| t.as_str()) != Some("user") {
            return None;
        }
        let text = value.pointer("/message/content")?.as_str()?;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        Some(trimmed.to_string())
    }

    fn ensure_cache(&self) -> Result<(), String> {
        let snapshot = self.load_snapshot()?;
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        *cache = Some(snapshot);
        Ok(())
    }

    fn project_path_from_uri(uri: &str) -> String {
        // project_uri is a file path like "/Users/maf/My Files/My apps/project"
        if uri.is_empty() {
            return String::new();
        }
        // Try to extract the last directory component as the project name
        Path::new(uri)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| uri.to_string())
    }

    fn ts_to_datetime(ts_millis: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(ts_millis / 1000, ((ts_millis % 1000) * 1_000_000) as u32)
            .unwrap_or_else(Utc::now)
    }

    fn infer_tool_name(params: &serde_json::Value) -> String {
        let obj = match params.as_object() {
            Some(o) => o,
            None => return "unknown_tool".to_string(),
        };

        if obj.contains_key("command") {
            return "Bash".to_string();
        }
        if obj.contains_key("file_path") {
            if obj.contains_key("original_text") || obj.contains_key("new_text") {
                return "SearchReplace".to_string();
            }
            if obj.contains_key("file_content") {
                return "Write".to_string();
            }
            return "Read".to_string();
        }
        if obj.contains_key("regex") || obj.contains_key("pattern") {
            return "Grep".to_string();
        }
        if obj.contains_key("query") {
            return "SearchCodebase".to_string();
        }
        if obj.contains_key("path") && obj.contains_key("query") {
            return "Glob".to_string();
        }
        if obj.contains_key("file_path") && obj.contains_key("start_line") {
            return "Read".to_string();
        }

        "tool".to_string()
    }

    fn extract_tool_input(params: &serde_json::Value) -> Option<String> {
        if params.is_null() {
            return None;
        }
        let s = serde_json::to_string_pretty(params).ok()?;
        Some(truncate_utf8(&s, 2000))
    }

    fn extract_tool_output(results: &serde_json::Value) -> Option<String> {
        if results.is_null() {
            return None;
        }
        // Try to extract meaningful content from results array
        if let Some(arr) = results.as_array() {
            let mut parts = Vec::new();
            for item in arr {
                if let Some(content) = item.get("content").and_then(|c| c.as_str()) {
                    if !content.is_empty() {
                        parts.push(truncate_utf8(content, 2000));
                    }
                }
            }
            if !parts.is_empty() {
                return Some(parts.join("\n---\n"));
            }
        }
        // Fallback: serialize the whole thing
        let s = serde_json::to_string(results).ok()?;
        if s.len() > 2 && s != "[]" && s != "{}" && s != "null" {
            Some(truncate_utf8(&s, 2000))
        } else {
            None
        }
    }
}

#[async_trait]
impl AgentAdapter for QoderAdapter {
    fn id(&self) -> &str {
        "qoder"
    }

    fn name(&self) -> &str {
        "Qoder"
    }

    async fn detect(&self) -> bool {
        Self::db_path().is_some()
    }

    async fn scan(&self) -> Vec<SessionLocation> {
        if let Err(e) = self.ensure_cache() {
            tracing::warn!("Failed to load Qoder DB: {}", e);
            return Vec::new();
        }

        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let snapshot = match cache.as_ref() {
            Some(s) => s,
            None => return Vec::new(),
        };

        snapshot
            .sessions
            .iter()
            .map(|row| SessionLocation {
                path: PathBuf::from(format!("qoder://session/{}", row.session_id)),
                last_modified: Self::ts_to_datetime(row.gmt_modified),
            })
            .collect()
    }

    async fn parse_session(&self, path: &Path) -> Result<NormalizedSession, String> {
        let path_str = path.to_string_lossy();
        let session_id = path_str
            .strip_prefix("qoder://session/")
            .ok_or_else(|| "Invalid qoder session path".to_string())?
            .to_string();

        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let snapshot = cache
            .as_ref()
            .ok_or_else(|| "Qoder DB not loaded".to_string())?;

        let session_row = snapshot
            .sessions
            .iter()
            .find(|s| s.session_id == session_id)
            .ok_or_else(|| format!("Session {} not found", session_id))?;

        let msg_rows = snapshot
            .messages
            .get(&session_id)
            .cloned()
            .unwrap_or_default();

        let project_path = Self::project_path_from_uri(&session_row.project_uri);
        let mut title = session_row.session_title.clone();
        let created_at = Self::ts_to_datetime(session_row.gmt_create);
        let updated_at = Self::ts_to_datetime(session_row.gmt_modified);

        let mut messages = Vec::new();
        let mut seq: u32 = 0;
        let mut first_user_msg = true;
        let mut input_tokens: u64 = 0;
        let mut output_tokens: u64 = 0;
        let mut cached_tokens: u64 = 0;

        // The DB `content` column is encrypted, but plaintext copies of the
        // user messages live in the transcript JSONL files. Match them by
        // timestamp (DB and transcript entries are written at nearly the
        // same instant); fall back to the session title for the first
        // message when no transcript entry matches.
        let user_query = if !title.is_empty() {
            Some(title.clone())
        } else {
            None
        };
        let mut transcript_user_msgs = snapshot
            .user_messages
            .get(&session_id)
            .cloned()
            .unwrap_or_default();

        for msg in &msg_rows {
            let ts = Self::ts_to_datetime(msg.gmt_create);

            match msg.role.as_str() {
                "user" => {
                    let content =
                        Self::take_matching_user_message(&mut transcript_user_msgs, msg.gmt_create)
                            .or_else(|| {
                                if first_user_msg {
                                    user_query.clone()
                                } else {
                                    None
                                }
                            });
                    if let Some(content) = content {
                        messages.push(Message {
                            id: uuid::Uuid::new_v4().to_string(),
                            session_id: session_id.clone(),
                            role: MessageRole::User,
                            content,
                            timestamp: Some(ts),
                            sequence: seq,
                            tool_name: None,
                            tool_input: None,
                            tool_output: None,
                        });
                        seq += 1;
                    }
                    first_user_msg = false;
                }
                "assistant" => {
                    // Assistant content is encrypted — skip to avoid noisy placeholders.
                    // Tool calls that follow carry the useful information.
                    // Token usage, however, is stored in plaintext token_info.
                    let (input, output, cached) = parse_token_info(msg.token_info.as_deref());
                    input_tokens = input_tokens.saturating_add(input);
                    output_tokens = output_tokens.saturating_add(output);
                    cached_tokens = cached_tokens.saturating_add(cached);
                }
                "tool" => {
                    if let Some(ref tool_result_json) = msg.tool_result {
                        if let Ok(tr) = serde_json::from_str::<serde_json::Value>(tool_result_json)
                        {
                            let params = tr
                                .get("parameters")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null);
                            let results = tr
                                .get("results")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null);
                            let tool_name = Self::infer_tool_name(&params);
                            let tool_input = Self::extract_tool_input(&params);
                            let tool_output = Self::extract_tool_output(&results);

                            messages.push(Message {
                                id: uuid::Uuid::new_v4().to_string(),
                                session_id: session_id.clone(),
                                role: MessageRole::Tool,
                                content: String::new(),
                                timestamp: Some(ts),
                                sequence: seq,
                                tool_name: Some(tool_name),
                                tool_input,
                                tool_output,
                            });
                            seq += 1;
                        }
                    }
                }
                _ => {}
            }
        }

        if title.is_empty() {
            title = format!(
                "Qoder Session ({})",
                &session_id.chars().take(12).collect::<String>()
            );
        }

        let session = Session {
            id: session_id,
            parent_session_id: None,
            agent: AgentType::Qoder,
            title,
            project_path,
            created_at,
            updated_at,
            file_path: path.to_string_lossy().to_string(),
            is_active: session_row.status == "Running",
            message_count: messages.len() as u32,
            input_tokens,
            output_tokens,
            cached_tokens,
            ..Default::default()
        };

        Ok(NormalizedSession {
            session,
            messages,
            attachments: Vec::new(),
            file_touches: vec![],
        })
    }

    fn resume_command(&self, _session_id: &str, _project_path: &str) -> String {
        if cfg!(target_os = "windows") {
            Self::windows_resume_command().to_string()
        } else {
            "open -a Qoder".to_string()
        }
    }

    async fn is_active(&self, session_path: &Path) -> bool {
        let path_str = session_path.to_string_lossy();
        if let Some(session_id) = path_str.strip_prefix("qoder://session/") {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(snapshot) = cache.as_ref() {
                return snapshot
                    .sessions
                    .iter()
                    .any(|s| s.session_id == session_id && s.status == "Running");
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_token_info_extracts_buckets_and_subtracts_cached() {
        let raw = r#"{"prompt_tokens":17604,"completion_tokens":226,"cached_tokens":13774,"max_input_tokens":180000}"#;
        let (input, output, cached) = parse_token_info(Some(raw));
        assert_eq!(input, 3830);
        assert_eq!(output, 226);
        assert_eq!(cached, 13774);
    }

    #[test]
    fn parse_token_info_handles_zero_cache() {
        let raw = r#"{"prompt_tokens":13777,"completion_tokens":185,"cached_tokens":0,"max_input_tokens":180000}"#;
        let (input, output, cached) = parse_token_info(Some(raw));
        assert_eq!(input, 13777);
        assert_eq!(output, 185);
        assert_eq!(cached, 0);
    }

    #[test]
    fn parse_token_info_returns_zeros_for_none_empty_or_malformed() {
        assert_eq!(parse_token_info(None), (0, 0, 0));
        assert_eq!(parse_token_info(Some("")), (0, 0, 0));
        assert_eq!(parse_token_info(Some("   ")), (0, 0, 0));
        assert_eq!(parse_token_info(Some("not json")), (0, 0, 0));
        assert_eq!(parse_token_info(Some("{}")), (0, 0, 0));
    }

    #[test]
    fn parse_token_info_saturates_when_cached_exceeds_prompt() {
        let raw = r#"{"prompt_tokens":10,"completion_tokens":5,"cached_tokens":20}"#;
        let (input, output, cached) = parse_token_info(Some(raw));
        assert_eq!(input, 0);
        assert_eq!(output, 5);
        assert_eq!(cached, 20);
    }

    #[test]
    fn extract_transcript_user_message_reads_plain_string_content() {
        let value = serde_json::json!({
            "type": "user",
            "sessionId": "sess-1",
            "message": {"role": "user", "content": "Fix the adapter"}
        });
        assert_eq!(
            QoderAdapter::extract_transcript_user_message(&value).as_deref(),
            Some("Fix the adapter")
        );
    }

    #[test]
    fn extract_transcript_user_message_ignores_tool_results_and_other_types() {
        let tool_result = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [{"type": "tool_result", "content": "out"}]}
        });
        assert_eq!(QoderAdapter::extract_transcript_user_message(&tool_result), None);

        let progress = serde_json::json!({"type": "progress", "data": {}});
        assert_eq!(QoderAdapter::extract_transcript_user_message(&progress), None);

        let empty = serde_json::json!({"type": "user", "message": {"role": "user", "content": "  "}});
        assert_eq!(QoderAdapter::extract_transcript_user_message(&empty), None);
    }

    #[test]
    fn scan_transcript_user_messages_collects_per_session() {
        let tmp = tempfile::tempdir().unwrap();
        let transcript_dir = tmp.path().join("-some-project").join("transcript");
        std::fs::create_dir_all(&transcript_dir).unwrap();
        std::fs::write(
            transcript_dir.join("sess-1.jsonl"),
            concat!(
                r#"{"type":"session_meta","sessionId":"sess-1","timestamp":"2026-08-13T16:09:44.003Z","data":{}}"#,
                "\n",
                r#"{"type":"user","sessionId":"sess-1","timestamp":"2026-08-13T16:09:44.004Z","message":{"role":"user","content":"First question"}}"#,
                "\n",
                r#"{"type":"user","sessionId":"sess-1","timestamp":"2026-08-13T16:10:00.000Z","message":{"role":"user","content":[{"type":"tool_result"}]}}"#,
                "\n",
                r#"{"type":"user","sessionId":"sess-1","timestamp":"2026-08-13T16:10:30.500Z","message":{"role":"user","content":"Follow up"}}"#,
                "\n",
            ),
        )
        .unwrap();
        // A project dir without a transcript/ subdir should be skipped.
        std::fs::create_dir_all(tmp.path().join("-empty-project")).unwrap();

        let map = QoderAdapter::scan_transcript_user_messages(tmp.path());
        assert_eq!(
            map.get("sess-1").map(|v| v.as_slice()),
            Some(
                [
                    (1_786_637_384_004, "First question".to_string()),
                    (1_786_637_430_500, "Follow up".to_string())
                ]
                .as_slice()
            )
        );
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn take_matching_user_message_pairs_by_closest_timestamp() {
        let mut candidates = vec![
            (1_000_000_000_000, "a".to_string()),
            (1_000_000_060_000, "b".to_string()),
        ];
        // Picks the closest candidate and removes it.
        assert_eq!(
            QoderAdapter::take_matching_user_message(&mut candidates, 1_000_000_060_002),
            Some("b".to_string())
        );
        assert_eq!(candidates.len(), 1);
        // No candidate within the tolerance window.
        assert_eq!(
            QoderAdapter::take_matching_user_message(&mut candidates, 2_000_000_000_000),
            None
        );
        assert_eq!(candidates.len(), 1);
    }

    fn session_row(session_id: &str, title: &str) -> QoderSessionRow {
        QoderSessionRow {
            session_id: session_id.to_string(),
            session_title: title.to_string(),
            project_uri: "/tmp/proj".to_string(),
            gmt_create: 1_700_000_000_000,
            gmt_modified: 1_700_000_100_000,
            status: "Success".to_string(),
        }
    }

    fn user_msg_row(session_id: &str, ts: i64) -> QoderMessageRow {
        QoderMessageRow {
            _id: format!("{}-{}", session_id, ts),
            role: "user".to_string(),
            tool_result: None,
            gmt_create: ts,
            token_info: None,
        }
    }

    #[tokio::test]
    async fn parse_session_prefers_transcript_user_messages_over_title() {
        let adapter = QoderAdapter::new();
        {
            let mut cache = adapter.cache.lock().unwrap();
            let mut messages = HashMap::new();
            messages.insert(
                "sess-1".to_string(),
                vec![
                    user_msg_row("sess-1", 1_700_000_000_000),
                    user_msg_row("sess-1", 1_700_000_050_000),
                ],
            );
            let mut user_messages = HashMap::new();
            user_messages.insert(
                "sess-1".to_string(),
                vec![
                    (1_700_000_000_001, "Actual first question".to_string()),
                    (1_700_000_049_999, "Actual follow up".to_string()),
                ],
            );
            *cache = Some(QoderDbSnapshot {
                sessions: vec![session_row("sess-1", "Generated Title")],
                messages,
                user_messages,
            });
        }

        let parsed = adapter
            .parse_session(Path::new("qoder://session/sess-1"))
            .await
            .unwrap();

        assert_eq!(parsed.session.title, "Generated Title");
        let user_msgs: Vec<&Message> = parsed
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::User)
            .collect();
        assert_eq!(user_msgs.len(), 2);
        assert_eq!(user_msgs[0].content, "Actual first question");
        assert_eq!(user_msgs[1].content, "Actual follow up");
    }

    #[tokio::test]
    async fn parse_session_aligns_partial_transcript_with_old_db_rows() {
        // The transcript may only cover recent turns of a long-lived session;
        // the plaintext message must pair with the matching DB row, not the
        // first one.
        let adapter = QoderAdapter::new();
        {
            let mut cache = adapter.cache.lock().unwrap();
            let mut messages = HashMap::new();
            messages.insert(
                "sess-3".to_string(),
                vec![
                    user_msg_row("sess-3", 1_690_000_000_000),
                    user_msg_row("sess-3", 1_700_000_050_000),
                ],
            );
            let mut user_messages = HashMap::new();
            user_messages.insert(
                "sess-3".to_string(),
                vec![(1_700_000_050_001, "Recent turn".to_string())],
            );
            *cache = Some(QoderDbSnapshot {
                sessions: vec![session_row("sess-3", "Session Title")],
                messages,
                user_messages,
            });
        }

        let parsed = adapter
            .parse_session(Path::new("qoder://session/sess-3"))
            .await
            .unwrap();

        let user_msgs: Vec<&Message> = parsed
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::User)
            .collect();
        assert_eq!(user_msgs.len(), 2);
        // Old row without transcript coverage falls back to the title.
        assert_eq!(user_msgs[0].content, "Session Title");
        assert_eq!(user_msgs[1].content, "Recent turn");
    }

    #[tokio::test]
    async fn parse_session_falls_back_to_title_without_transcript() {
        let adapter = QoderAdapter::new();
        {
            let mut cache = adapter.cache.lock().unwrap();
            let mut messages = HashMap::new();
            messages.insert("sess-2".to_string(), vec![user_msg_row("sess-2", 1_700_000_000_000)]);
            *cache = Some(QoderDbSnapshot {
                sessions: vec![session_row("sess-2", "Only Title Available")],
                messages,
                user_messages: HashMap::new(),
            });
        }

        let parsed = adapter
            .parse_session(Path::new("qoder://session/sess-2"))
            .await
            .unwrap();

        let user_msgs: Vec<&Message> = parsed
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::User)
            .collect();
        assert_eq!(user_msgs.len(), 1);
        assert_eq!(user_msgs[0].content, "Only Title Available");
    }
}

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::context::is_context_content;
use super::{AgentAdapter, PlatformPaths, SessionLocation};
use crate::models::*;

pub struct GrokAdapter;

impl Default for GrokAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize)]
struct GrokSummary {
    #[serde(default)]
    info: GrokSummaryInfo,
    #[serde(default)]
    session_summary: Option<String>,
    #[serde(default)]
    generated_title: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    last_active_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    current_model_id: Option<String>,
    #[serde(default)]
    head_branch: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct GrokSummaryInfo {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GrokActiveSession {
    session_id: String,
    pid: u32,
}

impl GrokAdapter {
    pub fn new() -> Self {
        Self
    }

    pub(crate) fn windows_home(paths: &PlatformPaths) -> Option<PathBuf> {
        paths.home_join(".grok")
    }

    pub(crate) fn windows_resume_command(session_id: &str, project_path: &str) -> String {
        let safe_path = crate::shell_quote::shell_quote(project_path);
        let safe_session = crate::shell_quote::shell_quote(session_id);
        format!("Set-Location {}; grok --resume {}", safe_path, safe_session)
    }

    fn grok_home() -> Option<PathBuf> {
        if let Ok(custom) = std::env::var("GROK_HOME") {
            let trimmed = custom.trim();
            if !trimmed.is_empty() {
                return Some(PathBuf::from(trimmed));
            }
        }
        if cfg!(target_os = "windows") {
            Self::windows_home(&PlatformPaths::system())
        } else {
            dirs::home_dir().map(|home| home.join(".grok"))
        }
    }

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

    pub(crate) fn scan_sessions_dir(sessions_dir: &Path) -> Vec<SessionLocation> {
        let mut locations = Vec::new();
        let Ok(project_entries) = std::fs::read_dir(sessions_dir) else {
            return locations;
        };

        for project_entry in project_entries.flatten() {
            let project_path = project_entry.path();
            if !project_path.is_dir() {
                continue;
            }
            let Ok(session_entries) = std::fs::read_dir(&project_path) else {
                continue;
            };
            for session_entry in session_entries.flatten() {
                let session_path = session_entry.path();
                if !session_path.is_dir() {
                    continue;
                }
                let chat_path = session_path.join("chat_history.jsonl");
                if chat_path.is_file() {
                    locations.push(SessionLocation {
                        last_modified: Self::modified_at(&chat_path),
                        path: chat_path,
                    });
                }
            }
        }

        locations
    }

    pub(crate) fn session_listed_as_active(
        active_json: &str,
        session_id: &str,
        pid_alive: impl Fn(u32) -> bool,
    ) -> bool {
        let Ok(rows) = serde_json::from_str::<Vec<GrokActiveSession>>(active_json) else {
            return false;
        };
        rows.iter()
            .any(|row| row.session_id == session_id && pid_alive(row.pid))
    }

    fn pid_is_alive(pid: u32) -> bool {
        if pid == 0 {
            return false;
        }

        #[cfg(unix)]
        {
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
        }

        #[cfg(windows)]
        {
            let output = std::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
                .output();
            match output {
                Ok(out) if out.status.success() => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    stdout.contains(&pid.to_string())
                }
                _ => false,
            }
        }

        #[cfg(not(any(unix, windows)))]
        {
            true
        }
    }

    fn session_id_from_chat_path(path: &Path) -> String {
        path.parent()
            .and_then(|dir| dir.file_name())
            .and_then(|name| name.to_str())
            .unwrap_or("unknown")
            .to_string()
    }
}

fn parse_rfc3339(value: Option<&str>) -> Option<DateTime<Utc>> {
    value
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(value) = u8::from_str_radix(hex, 16) {
                    out.push(value);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn extract_text_content(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| {
                let kind = block.get("type").and_then(|t| t.as_str()).unwrap_or("text");
                if kind == "text" || kind == "summary_text" {
                    block
                        .get("text")
                        .and_then(|t| t.as_str())
                        .map(str::to_string)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn extract_user_query_block(text: &str) -> Option<String> {
    let open = text.find("<user_query>")?;
    let after_open = open + "<user_query>".len();
    let close = text[after_open..].find("</user_query>")?;
    let inner = text[after_open..after_open + close].trim();
    if inner.is_empty() {
        None
    } else {
        Some(inner.to_string())
    }
}

fn file_operation_for(tool_name: &str) -> String {
    match tool_name {
        "read_file" | "Read" => "read".to_string(),
        "search_replace" | "str_replace" | "Edit" => "edit".to_string(),
        "write" | "Write" => "write".to_string(),
        _ => "unknown".to_string(),
    }
}

fn parse_tool_arguments(raw: &Value) -> Value {
    match raw {
        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
        other => other.clone(),
    }
}

fn extract_file_path(arguments: &Value) -> Option<String> {
    for key in ["target_file", "file_path", "path"] {
        if let Some(path) = arguments.get(key).and_then(|v| v.as_str()) {
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
    }
    None
}

#[async_trait]
impl AgentAdapter for GrokAdapter {
    fn id(&self) -> &str {
        "grok"
    }

    fn name(&self) -> &str {
        "Grok"
    }

    async fn detect(&self) -> bool {
        Self::grok_home().is_some_and(|home| home.exists())
    }

    async fn scan(&self) -> Vec<SessionLocation> {
        let Some(home) = Self::grok_home() else {
            return Vec::new();
        };
        Self::scan_sessions_dir(&home.join("sessions"))
    }

    async fn parse_session(&self, path: &Path) -> Result<NormalizedSession, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read Grok session file: {e}"))?;

        let summary = path
            .parent()
            .map(|dir| dir.join("summary.json"))
            .and_then(|summary_path| std::fs::read_to_string(summary_path).ok())
            .and_then(|raw| serde_json::from_str::<GrokSummary>(&raw).ok());

        let mut session_id = Self::session_id_from_chat_path(path);
        let mut project_path = path
            .parent()
            .and_then(|session_dir| session_dir.parent())
            .and_then(|encoded| encoded.file_name())
            .and_then(|name| name.to_str())
            .map(percent_decode)
            .unwrap_or_else(|| "unknown".to_string());
        let mut title = String::new();
        let mut model = None;
        let mut git_branch = None;
        let mut created_at = Self::modified_at(path);
        let mut updated_at = created_at;

        if let Some(summary) = &summary {
            if let Some(id) = summary.info.id.clone() {
                session_id = id;
            }
            if let Some(cwd) = summary.info.cwd.clone() {
                project_path = cwd;
            }
            title = summary
                .generated_title
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| summary.session_summary.clone().filter(|s| !s.is_empty()))
                .unwrap_or_default();
            model = summary.current_model_id.clone();
            git_branch = summary.head_branch.clone();
            if let Some(ts) = parse_rfc3339(summary.created_at.as_deref()) {
                created_at = ts;
            }
            if let Some(ts) = parse_rfc3339(
                summary
                    .last_active_at
                    .as_deref()
                    .or(summary.updated_at.as_deref()),
            ) {
                updated_at = ts;
            }
        }

        let mut messages = Vec::new();
        let mut file_touches = Vec::new();
        let mut tool_index_by_id: HashMap<String, usize> = HashMap::new();
        let mut seq: u32 = 0;

        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let json: Value = match serde_json::from_str(line) {
                Ok(value) => value,
                Err(_) => continue,
            };

            let entry_type = json.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match entry_type {
                "reasoning" | "backend_tool_call" => continue,
                "system" => {
                    let text = extract_text_content(json.get("content").unwrap_or(&Value::Null));
                    if text.is_empty() {
                        continue;
                    }
                    messages.push(Message {
                        id: uuid::Uuid::new_v4().to_string(),
                        session_id: session_id.clone(),
                        role: MessageRole::Context,
                        content: text,
                        timestamp: None,
                        sequence: seq,
                        tool_name: None,
                        tool_input: None,
                        tool_output: None,
                    });
                    seq += 1;
                }
                "user" => {
                    let raw_text =
                        extract_text_content(json.get("content").unwrap_or(&Value::Null));
                    if raw_text.is_empty() {
                        continue;
                    }

                    let synthetic = json
                        .get("synthetic_reason")
                        .and_then(|v| v.as_str())
                        .is_some();
                    let user_query = extract_user_query_block(&raw_text);
                    let is_context = synthetic
                        || is_context_content(&raw_text)
                        || raw_text.trim_start().starts_with("<user_info>");

                    if is_context && user_query.is_none() {
                        messages.push(Message {
                            id: uuid::Uuid::new_v4().to_string(),
                            session_id: session_id.clone(),
                            role: MessageRole::Context,
                            content: raw_text,
                            timestamp: None,
                            sequence: seq,
                            tool_name: None,
                            tool_input: None,
                            tool_output: None,
                        });
                        seq += 1;
                        continue;
                    }

                    let content = user_query.unwrap_or(raw_text);
                    if title.is_empty() && !content.is_empty() {
                        title = content.chars().take(100).collect();
                    }
                    messages.push(Message {
                        id: uuid::Uuid::new_v4().to_string(),
                        session_id: session_id.clone(),
                        role: MessageRole::User,
                        content,
                        timestamp: None,
                        sequence: seq,
                        tool_name: None,
                        tool_input: None,
                        tool_output: None,
                    });
                    seq += 1;
                }
                "assistant" => {
                    if model.is_none() {
                        model = json
                            .get("model_id")
                            .and_then(|v| v.as_str())
                            .map(ToString::to_string);
                    }

                    let content_text =
                        extract_text_content(json.get("content").unwrap_or(&Value::Null));
                    if !content_text.is_empty() {
                        messages.push(Message {
                            id: uuid::Uuid::new_v4().to_string(),
                            session_id: session_id.clone(),
                            role: MessageRole::Assistant,
                            content: content_text,
                            timestamp: None,
                            sequence: seq,
                            tool_name: None,
                            tool_input: None,
                            tool_output: None,
                        });
                        seq += 1;
                    }

                    if let Some(tool_calls) = json.get("tool_calls").and_then(|v| v.as_array()) {
                        for call in tool_calls {
                            let name = call
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let args = call
                                .get("arguments")
                                .map(parse_tool_arguments)
                                .unwrap_or(Value::Null);
                            let tool_input = if args.is_null() {
                                None
                            } else {
                                serde_json::to_string(&args).ok()
                            };

                            if let Some(file_path) = extract_file_path(&args) {
                                file_touches.push(FileTouch {
                                    path: file_path,
                                    operation: file_operation_for(&name),
                                    sequence: seq,
                                });
                            }

                            let message_index = messages.len();
                            messages.push(Message {
                                id: uuid::Uuid::new_v4().to_string(),
                                session_id: session_id.clone(),
                                role: MessageRole::Tool,
                                content: String::new(),
                                timestamp: None,
                                sequence: seq,
                                tool_name: Some(name),
                                tool_input,
                                tool_output: None,
                            });
                            if let Some(call_id) = call.get("id").and_then(|v| v.as_str()) {
                                tool_index_by_id.insert(call_id.to_string(), message_index);
                            }
                            seq += 1;
                        }
                    }
                }
                "tool_result" => {
                    let output = extract_text_content(json.get("content").unwrap_or(&Value::Null));
                    if let Some(call_id) = json.get("tool_call_id").and_then(|v| v.as_str()) {
                        if let Some(index) = tool_index_by_id.get(call_id).copied() {
                            messages[index].tool_output = Some(output);
                            continue;
                        }
                    }
                    if let Some(msg) = messages
                        .iter_mut()
                        .rev()
                        .find(|msg| msg.role == MessageRole::Tool && msg.tool_output.is_none())
                    {
                        msg.tool_output = Some(output);
                    }
                }
                _ => {}
            }
        }

        if title.is_empty() {
            title = format!(
                "Grok Session {}",
                session_id.chars().take(8).collect::<String>()
            );
        }

        Ok(NormalizedSession {
            session: Session {
                id: session_id,
                parent_session_id: None,
                agent: AgentType::Grok,
                title,
                project_path,
                created_at,
                updated_at,
                file_path: path.to_string_lossy().to_string(),
                is_active: false,
                message_count: messages.len() as u32,
                model,
                git_branch,
                input_tokens: 0,
                output_tokens: 0,
                cached_tokens: 0,
                reasoning_tokens: 0,
                file_count: 0,
            },
            messages,
            attachments: Vec::new(),
            file_touches,
        })
    }

    fn resume_command(&self, session_id: &str, project_path: &str) -> String {
        if cfg!(target_os = "windows") {
            return Self::windows_resume_command(session_id, project_path);
        }

        let safe_path = crate::shell_quote::shell_quote(project_path);
        let safe_session = crate::shell_quote::shell_quote(session_id);
        format!("cd {} && grok --resume {}", safe_path, safe_session)
    }

    async fn is_active(&self, session_path: &Path) -> bool {
        let Some(home) = Self::grok_home() else {
            return false;
        };
        let session_id = Self::session_id_from_chat_path(session_path);
        let Ok(active_json) = std::fs::read_to_string(home.join("active_sessions.json")) else {
            return false;
        };
        Self::session_listed_as_active(&active_json, &session_id, Self::pid_is_alive)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::AgentAdapter;
    use std::fs;

    fn write_grok_session(
        root: &Path,
        cwd: &str,
        session_id: &str,
        summary: &str,
        chat_history: &str,
    ) -> PathBuf {
        let encoded = percent_encode_path(cwd);
        let session_dir = root.join("sessions").join(encoded).join(session_id);
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(session_dir.join("summary.json"), summary).unwrap();
        let chat_path = session_dir.join("chat_history.jsonl");
        fs::write(&chat_path, chat_history).unwrap();
        chat_path
    }

    fn percent_encode_path(path: &str) -> String {
        path.bytes()
            .flat_map(|b| {
                if b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_' {
                    vec![b as char]
                } else {
                    format!("%{b:02X}").chars().collect()
                }
            })
            .collect()
    }

    #[tokio::test]
    async fn parse_session_reads_summary_and_conversation_turns() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = "019ff9f7-101c-7771-8df8-0a04c62fd178";
        let cwd = "/Users/maf/My Files/My apps/thedarts.co/orbit";
        let summary = r#"{
            "info": {"id": "019ff9f7-101c-7771-8df8-0a04c62fd178", "cwd": "/Users/maf/My Files/My apps/thedarts.co/orbit"},
            "session_summary": "Add GrokCli Adapter Integration",
            "created_at": "2026-08-13T07:12:26.240864Z",
            "updated_at": "2026-08-13T07:15:54.291494Z",
            "current_model_id": "grok-4.6",
            "head_branch": "main",
            "last_active_at": "2026-08-13T07:15:54.291494Z",
            "generated_title": "Add GrokCli Adapter Integration"
        }"#;
        let chat = concat!(
            r#"{"type":"system","content":"You are Grok 4.6 released by xAI."}"#,
            "\n",
            r#"{"type":"user","content":[{"type":"text","text":"<user_info>\nOS Version: macos\n</user_info>"}]}"#,
            "\n",
            r#"{"type":"user","content":[{"type":"text","text":"<system-reminder>\nMCP servers connected\n</system-reminder>"}],"synthetic_reason":"system_reminder"}"#,
            "\n",
            r#"{"type":"user","content":[{"type":"text","text":"<user_query>\nwe need to add GrokCli Adapter for this app\n</user_query>"}],"prompt_index":0}"#,
            "\n",
            r#"{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"thinking"}],"encrypted_content":"abc","status":"completed"}"#,
            "\n",
            r#"{"type":"assistant","content":"I'll start by loading the project skills.","tool_calls":[{"id":"call-1","name":"read_file","arguments":"{\"target_file\":\"/tmp/src/main.rs\"}"}],"model_id":"grok-4.6"}"#,
            "\n",
            r#"{"type":"tool_result","tool_call_id":"call-1","content":"fn main() {}"}"#,
            "\n",
            r#"{"type":"backend_tool_call","kind":{"name":"hidden"}}"#,
            "\n",
        );
        let path = write_grok_session(tmp.path(), cwd, session_id, summary, chat);

        let adapter = GrokAdapter::new();
        let parsed = adapter.parse_session(&path).await.unwrap();

        assert_eq!(parsed.session.id, session_id);
        assert_eq!(parsed.session.agent, AgentType::Grok);
        assert_eq!(parsed.session.title, "Add GrokCli Adapter Integration");
        assert_eq!(parsed.session.project_path, cwd);
        assert_eq!(parsed.session.model.as_deref(), Some("grok-4.6"));
        assert_eq!(parsed.session.git_branch.as_deref(), Some("main"));
        assert_eq!(
            parsed.session.created_at.to_rfc3339(),
            "2026-08-13T07:12:26.240864+00:00"
        );

        assert_eq!(parsed.messages.len(), 6);
        assert_eq!(parsed.messages[0].role, MessageRole::Context);
        assert!(parsed.messages[0].content.contains("You are Grok"));
        assert_eq!(parsed.messages[1].role, MessageRole::Context);
        assert!(parsed.messages[1].content.starts_with("<user_info>"));
        assert_eq!(parsed.messages[2].role, MessageRole::Context);
        assert!(parsed.messages[2].content.starts_with("<system-reminder>"));
        assert_eq!(parsed.messages[3].role, MessageRole::User);
        assert_eq!(
            parsed.messages[3].content,
            "we need to add GrokCli Adapter for this app"
        );
        assert_eq!(parsed.messages[4].role, MessageRole::Assistant);
        assert_eq!(
            parsed.messages[4].content,
            "I'll start by loading the project skills."
        );
        assert_eq!(parsed.messages[5].role, MessageRole::Tool);
        assert_eq!(parsed.messages[5].tool_name.as_deref(), Some("read_file"));
        assert_eq!(
            parsed.messages[5].tool_output.as_deref(),
            Some("fn main() {}")
        );

        assert_eq!(parsed.file_touches.len(), 1);
        assert_eq!(parsed.file_touches[0].path, "/tmp/src/main.rs");
        assert_eq!(parsed.file_touches[0].operation, "read");
    }

    #[tokio::test]
    async fn parse_session_falls_back_when_summary_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let cwd = "/tmp/demo-project";
        let encoded = percent_encode_path(cwd);
        let session_dir = tmp.path().join("sessions").join(&encoded).join(session_id);
        fs::create_dir_all(&session_dir).unwrap();
        let chat_path = session_dir.join("chat_history.jsonl");
        fs::write(
            &chat_path,
            r#"{"type":"user","content":[{"type":"text","text":"<user_query>hello from grok</user_query>"}]}"#,
        )
        .unwrap();

        let parsed = GrokAdapter::new().parse_session(&chat_path).await.unwrap();
        assert_eq!(parsed.session.id, session_id);
        assert_eq!(parsed.session.title, "hello from grok");
        assert_eq!(parsed.session.project_path, cwd);
        assert_eq!(parsed.messages[0].role, MessageRole::User);
        assert_eq!(parsed.messages[0].content, "hello from grok");
    }

    #[test]
    fn scan_sessions_dir_finds_chat_history_and_skips_sidecars() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = "/tmp/demo-project";
        let session_id = "bbbbbbbb-cccc-dddd-eeee-ffffffffffff";
        let chat = write_grok_session(
            tmp.path(),
            cwd,
            session_id,
            r#"{"info":{"id":"bbbbbbbb-cccc-dddd-eeee-ffffffffffff","cwd":"/tmp/demo-project"}}"#,
            r#"{"type":"user","content":"hi"}"#,
        );
        let project_dir = chat.parent().unwrap().parent().unwrap();
        fs::write(project_dir.join("prompt_history.jsonl"), "{}\n").unwrap();
        fs::write(
            tmp.path().join("sessions").join("session_search.sqlite"),
            [],
        )
        .unwrap();

        let locations = GrokAdapter::scan_sessions_dir(&tmp.path().join("sessions"));
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].path, chat);
    }

    #[test]
    fn windows_home_is_under_userprofile_grok() {
        let paths = PlatformPaths {
            home: Some(PathBuf::from(r"C:\Users\orbit")),
            data: Some(PathBuf::from(r"C:\Users\orbit\AppData\Roaming")),
            data_local: Some(PathBuf::from(r"C:\Users\orbit\AppData\Local")),
        };
        assert_eq!(
            GrokAdapter::windows_home(&paths),
            Some(PathBuf::from(r"C:\Users\orbit").join(".grok"))
        );
    }

    #[test]
    fn active_session_matches_live_pid() {
        let json = r#"[{"session_id":"sess-1","pid":1234,"cwd":"/tmp"}]"#;
        assert!(GrokAdapter::session_listed_as_active(
            json,
            "sess-1",
            |_| true
        ));
        assert!(!GrokAdapter::session_listed_as_active(
            json,
            "sess-1",
            |_| false
        ));
        assert!(!GrokAdapter::session_listed_as_active(
            json,
            "sess-2",
            |_| true
        ));
    }

    #[tokio::test]
    async fn parse_real_grok_home_session_when_present() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let sessions = home.join(".grok").join("sessions");
        if !sessions.is_dir() {
            return;
        }
        let Some(chat) = GrokAdapter::scan_sessions_dir(&sessions)
            .into_iter()
            .next()
            .map(|loc| loc.path)
        else {
            return;
        };

        let parsed = GrokAdapter::new().parse_session(&chat).await.unwrap();
        assert_eq!(parsed.session.agent, AgentType::Grok);
        assert!(!parsed.session.id.is_empty());
        assert!(!parsed.session.title.is_empty());
        assert!(
            parsed
                .messages
                .iter()
                .any(|message| message.role == MessageRole::User),
            "expected at least one user turn in {chat:?}"
        );
        assert!(
            parsed
                .messages
                .iter()
                .all(|message| message.role != MessageRole::System),
            "system prompt should be Context, not System"
        );
    }
}

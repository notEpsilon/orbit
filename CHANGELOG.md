# Changelog

## v0.11.0

### Search

- **FTS5-powered session search** — queries now run against the `messages_fts` full-text index instead of a brute-force LIKE scan over every message's content, tool input, and tool output
- Token + word-prefix matching: `auth` finds "authentication" (mid-word substrings like `rror` no longer match — the trade-off for indexed, ranked search)
- Multi-word queries: every term must match somewhere in the session, and terms may hit different messages
- Relevance ordering while searching: title matches first, then best-message bm25 rank, then recency
- Automatic fallback to the previous LIKE scan if the FTS query fails

### Technical

- `get_sessions` builds a safe FTS5 MATCH expression from raw input (quoted prefix terms neutralize FTS query syntax such as `(`, `*`, `NEAR`)
- Ranking CTE computes each session's best bm25 rank; per-term subqueries enforce session-level AND semantics
- Removed the unused `search_sessions` command and `search_messages` query
- Parameterized `LIMIT`/`OFFSET` in `get_sessions`
- Version bumped to 0.11.0

## v0.10.0

### New Agent Adapter

- **DeepSeek Harness** — indexes `dsh` sessions from the harness home (`$DSH_HOME/sessions` or `~/.dsh/sessions`); supports both installation methods: npm (`npx @deepseek-ai/dsh web`) and local source builds (`pnpm dsh web`)
- Reads the harness's session logs (`session.jsonl` and the default Zstandard-compressed `session.jsonl.zstd`), tolerating crash-orphaned torn frames
- Conversation-focused parsing: user prompts, assistant replies (text only; reasoning excluded), and paired tool calls/results with file-touch extraction
- Classifies harness-injected scaffolding (runtime-context snapshots, AGENTS.md preambles, skill content) as Orbit's Context role via `source.kind`
- Titles from the harness's own `session/title` events (LLM-generated titles win), plus model and token usage from the log
- Resume smartly resolves the active installation: `dsh web` when the binary is on `$PATH`, `pnpm dsh web` from a discovered local checkout, else `npx @deepseek-ai/dsh web`

### Fixes

- **Qoder** — show real user messages from plaintext transcripts instead of the session title (Qoder DB user content is encrypted)
- **Codex** — retain cumulative token snapshots so token totals stay accurate across re-indexes

### Documentation

- Added DeepSeek Harness to the supported-agents table

### Technical

- Added `DshAdapter` with JSONL + zstd parsing, session-event decoding, tool-call/result pairing, and install-aware multi-platform resume
- Added `zstd` dependency for decompressing compressed session logs
- Registered `dsh` in backend `AgentType`, frontend labels/colors, and adapter registry
- Version bumped to 0.10.0

## v0.9.0

### New Agent Adapter

- **Grok** — indexes official xAI Grok Build CLI sessions from `~/.grok/sessions/` (or `$GROK_HOME/sessions`); reads `summary.json` metadata and `chat_history.jsonl` transcripts
- Conversation-focused parsing: user queries, assistant replies, and tool calls/results
- Classifies Grok scaffolding (`<user_info>`, `<system-reminder>`, system prompt) as Orbit's Context role
- Skips encrypted reasoning blobs and backend tool-call records
- Resume via `grok --resume <session-id>`; active sessions from `active_sessions.json` plus live PID
- Windows discovery under `%USERPROFILE%\.grok` with a copyable PowerShell resume command

### Documentation

- Added Grok to the supported-agents table and Linux local-dev platform notes

### Technical

- Added `GrokAdapter` with JSONL + summary parsing, file-touch extraction, and multi-platform resume
- Extended shared context detection for Grok `<user_info>` preambles
- Registered `grok` in backend `AgentType`, frontend labels/colors, and parser version `1`
- Version bumped to 0.9.0

## v0.6.0

### New Agent Adapters

- **Kilo Code** — parses JSONL session files from Kilo Code (VS Code extension + CLI); supports macOS, Linux, and Windows discovery with full resume support
- **ZCode** — parses JSON session files from `~/.zcode/cli/rollout/` with tail-window merging; supports macOS, Linux, and Windows discovery

### Statistics Dashboard

- New **model statistics** dashboard with aggregate model usage, session counts, and timeline data
- **Period-based filtering** for weekly, monthly, and yearly breakdowns
- **Formatted timeline labels** that adjust by time period granularity

### Session Browsing

- **Accordion session collapse** — parent sessions with sub-sessions can be collapsed/expanded in the session list
- **Warp sub-session linking** — Warp sub-sessions (tool calls) are now associated with their parent session via protobuf field 3 parsing

### Fixes

- **Cursor project path deduplication** — merged variant cursor project paths to prevent duplicate sessions
- **Statistics timeline labels** — fixed label formatting by period granularity

### Technical

- Added `KiloCodeAdapter` with CLI DB active-session tracking and multi-platform resume (cd + kilo --resume on Unix, PowerShell-compatible on Windows)
- Added `ZCodeAdapter` with JSON session parsing, tool-call extraction, and tail-window deduplication
- Added `StatisticsAggregator` backend with SQL aggregation queries for model stats and timeline data
- Added statistics dashboard UI components with Tailwind-styled charts
- Updated Warp adapter to parse protobuf field 3 for parent-child session associations
- Updated session list virtualization to support accordion collapse/expand
- Version bumped to 0.6.0

## v0.5.0

### Linux Support

- **Linux adapter discovery** — Claude Code, Codex, Cursor, and OpenCode session discovery enabled on Linux
- **Linux resume terminals** — session resume works on Linux with Terminal.app-style terminal detection; supports GNOME Console, GNOME Terminal, Konsole, and xterm
- **Linux AppImage build** — local AppImage generation with build and verification scripts

### Fixes

- **OpenCode SQLite sessions** — fixed parsing of newer OpenCode versions that use SQLite-backed storage on macOS
- **Codex resume command** — updated to match current Codex CLI resume syntax
- **Linux terminal detection** — tightened terminal precedence ordering for reliable resume
- **Adapter ordering** — reordered ALL_AGENTS array for consistent UI filter chip order

### Documentation

- Documented local Linux AppImage build process, platform data paths, and adapter discovery scope

### Technical

- Added `PlatformPaths` helper for OS-agnostic filesystem path resolution
- Added shell quoting utility (`shell_quote`) for safe command construction across platforms
- Added AppImage build script (`build-linux-appimage.sh`) with integrity verification
- Extended `Terminal` enum with Linux terminal variants and auto-detection logic
- Version bumped to 0.5.0

## v0.4.0

### New Agent Adapter

- **Antigravity** — parses JSONL transcripts from `~/.gemini/antigravity/brain/`; extracts user requests from `<USER_REQUEST>` blocks, planner responses with thinking, tool calls with file operations, and token usage estimates

## v0.1.0 — Initial Release

First public release of Orbit, a native desktop app for browsing AI coding agent session history.

### Agent Adapters

- **Claude Code** — scans `~/.claude/projects/` JSONL files; filters out Claude-Mem plugin/subagent sessions automatically
- **Codex** — scans `~/.codex/` JSONL sessions
- **Cursor** — parses Anthropic-style JSONL transcripts from `~/.cursor/projects/`; infers project paths from encoded directory names
- **OpenCode** — supports three storage formats: legacy JSONL (`sessions/`), current storage layout (`storage/session/`), and database-backed sessions via `opencode.db`
- **Warp** — reads protobuf-encoded agent tasks from Warp's local SQLite database
- **GitHub Copilot CLI** — parses conversation history from Copilot's local storage
- **Qoder** — scans Qoder session files with full transcript parsing

### Session Indexing

- Automatic detection of installed agents on macOS
- SQLite database with WAL mode for the local index
- FTS5 full-text search index on message content, tool inputs, and tool outputs
- Change detection via size + mtime hashing to skip unchanged session files
- Parser versioning to force re-parse when adapter logic changes
- Stale session cleanup after each full scan
- Per-provider sync stats tracking

### Session Browsing

- Searchable, filterable session list with virtual scrolling for large histories
- Filter by agent type, project path, model, git branch, and active status
- Multi-agent filter chips for quick toggling
- Search highlights in session titles and transcript content
- Session metadata display: message count, file count, token usage, model, git branch

### Transcript Viewer

- Chat-style transcript with user, assistant, and tool messages
- Markdown rendering with syntax highlighting (via `react-markdown` + `rehype-highlight`)
- Collapsible tool call blocks showing tool name, inputs, and outputs
- Message role filter (All / User / Assistant / Tool)
- In-transcript search with match navigation
- Virtualized rendering for long transcripts

### Resume Sessions

- Copy resume command to clipboard
- Launch resume directly in your preferred terminal (Terminal.app, iTerm2, Warp, Ghostty)
- Configurable preferred terminal in settings

### UI

- Dark theme with custom design tokens
- Resizable sidebar
- Active session indicators with 5-second polling
- Sync status modal showing per-provider indexing stats and last sync time
- Custom app icon

### Technical Stack

- **Frontend**: React 19, TypeScript, Tailwind CSS v4, Vite, Zustand, TanStack Virtual
- **Backend**: Rust, Tauri v2, SQLite (rusqlite), FTS5, protobuf (prost)
- **Platform**: macOS-first (Linux and Windows builds not tested)

### Known Limitations

- macOS is the only tested platform
- App bundles are not signed or notarized
- Live file watching is defined but not wired into the app loop
- Session formats may change when agent vendors update their tools

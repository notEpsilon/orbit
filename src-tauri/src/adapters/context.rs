//! Shared detection of agent-injected "context" messages — the scaffolding
//! that tools emit around the real conversation (environment summaries,
//! recommended plugins, AGENTS.md preambles, slash-command metadata, command
//! caveats, etc.). These look like user/system turns in the raw transcripts but
//! are not genuine user intent, so adapters reclassify them to
//! [`crate::models::MessageRole::Context`] for grouping and filtering.

/// Returns true when `text` is an agent-injected context message rather than a
/// real turn. Detection is by leading tag/prefix only (after trimming), so it
/// never matches a genuine message that merely *mentions* one of these tags.
///
/// Covers the agents Orbit supports today:
/// - **Codex**: `<environment_context>`, `<recommended_plugins>`,
///   `<user_instructions>`, `<permissions instructions>`, `<turn_aborted>`,
///   and the `# AGENTS.md ...` / `# Context from my IDE setup` /
///   `# Codebase Context` markdown preambles.
/// - **Claude**: `<command-name>`, `<command-message>`, `<command-args>`,
///   `<local-command-caveat>`, `<local-command-stdout>`,
///   `<local-command-stderr>`, `<system-reminder>`.
/// - **Grok**: `<user_info>` workspace preamble (plus Claude's
///   `<system-reminder>` tags, which Grok also emits).
pub fn is_context_content(text: &str) -> bool {
    let trimmed = text.trim_start();

    // --- Codex XML-style preambles ---
    trimmed.starts_with("<environment_context>")
        || trimmed.starts_with("<recommended_plugins>")
        || trimmed.starts_with("<user_instructions>")
        || trimmed.starts_with("<permissions instructions>")
        || trimmed.starts_with("<turn_aborted>")
        // --- Codex markdown preambles ---
        || trimmed.starts_with("# AGENTS.md instructions")
        || trimmed.starts_with("# AGENTS.md from")
        || trimmed.starts_with("# Context from my IDE setup")
        || trimmed.starts_with("# Codebase Context")
        // --- Claude command / local-command / system scaffolding ---
        || trimmed.starts_with("<command-name>")
        || trimmed.starts_with("<command-message>")
        || trimmed.starts_with("<command-args>")
        || trimmed.starts_with("<local-command-caveat>")
        || trimmed.starts_with("<local-command-stdout>")
        || trimmed.starts_with("<local-command-stderr>")
        || trimmed.starts_with("<system-reminder>")
        // --- Grok CLI injected workspace preamble ---
        || trimmed.starts_with("<user_info>")
}

#[cfg(test)]
mod tests {
    use super::is_context_content;

    #[test]
    fn detects_codex_xml_preambles() {
        assert!(is_context_content("<environment_context> <cwd>/x</cwd>"));
        assert!(is_context_content(
            "<recommended_plugins>\nAtlassian Rovo\nGmail\n"
        ));
        assert!(is_context_content(
            "<user_instructions>\ndo the thing\n</user_instructions>"
        ));
        assert!(is_context_content("<permissions instructions> ..."));
        assert!(is_context_content("<turn_aborted>"));
    }

    #[test]
    fn detects_codex_markdown_preambles() {
        assert!(is_context_content("# AGENTS.md instructions for repo"));
        assert!(is_context_content("# AGENTS.md from upstream"));
        assert!(is_context_content("# Context from my IDE setup"));
        assert!(is_context_content("# Codebase Context"));
    }

    #[test]
    fn detects_claude_command_scaffolding() {
        assert!(is_context_content("<local-command-caveat>..."));
        assert!(is_context_content("<command-name>/model</command-name>"));
        assert!(is_context_content(
            "<command-message>model</command-message>"
        ));
        assert!(is_context_content("<command-args></command-args>"));
        assert!(is_context_content(
            "<local-command-stdout>ok</local-command-stdout>"
        ));
        assert!(is_context_content(
            "<local-command-stderr>err</local-command-stderr>"
        ));
        assert!(is_context_content(
            "<system-reminder>follow the plan</system-reminder>"
        ));
    }

    #[test]
    fn detects_grok_user_info_scaffolding() {
        assert!(is_context_content(
            "<user_info>\nOS Version: macos\nWorkspace Path: /tmp/orbit\n</user_info>"
        ));
    }

    #[test]
    fn ignores_leading_whitespace() {
        assert!(is_context_content("   \n\t<environment_context>"));
    }

    #[test]
    fn does_not_flag_real_messages() {
        assert!(!is_context_content("Fix the login bug please"));
        assert!(!is_context_content("What does <command-name> mean?"));
        // A message that merely *mentions* a tag (not starting with it) is real.
        assert!(!is_context_content(
            "I saw <recommended_plugins> in the log"
        ));
    }

    #[test]
    fn does_not_flag_empty_or_plain_text() {
        assert!(!is_context_content(""));
        assert!(!is_context_content("   "));
        assert!(!is_context_content("hello world"));
    }
}

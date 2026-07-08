//! Micro Compact — lightweight per-tool-result compaction that runs before
//! each API call.  Inspired by claude-code's `microCompact.ts`.
//!
//! Instead of waiting for the full conversation to hit the auto-compact
//! threshold, micro-compact proactively clears the **content** of old
//! tool results (read_file, grep, bash, …) while keeping the structural
//! skeleton (tool_use_id, tool_name) so the API still sees a valid
//! message sequence.
//!
//! Cleared results are replaced with a short placeholder:
//!   `[Old tool result content cleared]`
//!
//! This dramatically shrinks the request body for providers with strict
//! payload limits (e.g. GLM's gateway returning 400 on oversized bodies).

use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session};

/// Placeholder substituted for cleared tool-result content.
pub const CLEARED_PLACEHOLDER: &str = "[Old tool result content cleared]";

/// Tool names whose results are eligible for micro-compact.
/// Mirrors claude-code's COMPACTABLE_TOOLS set.
const COMPACTABLE_TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "glob_search",
    "grep_search",
    "search_replace",
    "bash",
    "PowerShell",
    "WebFetch",
    "WebSearch",
    "list_directory",
];

/// Minimum character length for a tool result to be considered for clearing.
/// Short results (e.g. "ok", "file written") are not worth compacting.
///
/// Set high (5 KB) so micro-compact only clears results that are genuinely
/// large — a small result carries information worth more than the tokens it
/// costs, and clearing it forces the model to re-read the file, which is the
/// observed "降智" failure mode.  Only bulky outputs (big file reads, long
/// grep/bash output) get cleared.
const MIN_OUTPUT_LENGTH_FOR_CLEAR: usize = 5_000;

/// Emergency threshold: tool results larger than this are ALWAYS cleared,
/// regardless of whether they're in the protected recent window.
/// This prevents context window explosion from huge outputs (e.g., a 2MB
/// grep result would exceed the context window by itself).
const EMERGENCY_CLEAR_THRESHOLD: usize = 500_000;

/// How many **recent** tool results to protect from micro-compact, regardless
/// of which assistant turn they belong to.  Mirrors claude-code's
/// `keepRecent` semantics (default 5 in timeBasedMCConfig.ts) but raised to
/// 8 for claw-code: GLM has no server-side cache_edits, so the model has no
/// way to recover cleared content except by re-running the tool.  A wider
/// protection window trades slightly larger requests for much better answer
/// quality on multi-tool problems.
const PROTECT_RECENT_TOOL_RESULTS: usize = 8;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Result of running micro-compact on a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicroCompactResult {
    /// Number of tool results that were cleared.
    pub cleared_count: usize,
    /// Total characters freed by clearing.
    pub chars_freed: usize,
}

/// Runs micro-compact on the session, clearing old compactable tool
/// results in-place.  Returns a summary of what was cleared.
///
/// This should be called **before** each API request is built, so the
/// request body stays small.
///
/// Protection model: the most recent `PROTECT_RECENT_TOOL_RESULTS` compactable
/// tool results are always kept verbatim (regardless of which turn they belong
/// to).  Only results older than that window — and larger than
/// `MIN_OUTPUT_LENGTH_FOR_CLEAR` — are cleared.  This preserves enough working
/// context for the model to reason across several tool calls, which is
/// essential when the provider has no server-side cache_edits to fall back on.
pub fn microcompact_session(session: &mut Session) -> MicroCompactResult {
    let mut cleared_count = 0;
    let mut chars_freed = 0;
    let mut cleared_tools: Vec<String> = Vec::new();

    // Collect the IDs of the most recent N compactable tool results (newest
    // first), then keep them verbatim.
    let recent_ids =
        collect_recent_compactable_ids(&session.messages, PROTECT_RECENT_TOOL_RESULTS);
    let protect_set: std::collections::HashSet<&str> =
        recent_ids.iter().map(|s| s.as_str()).collect();

    // Walk all messages and clear compactable tool results that are NOT in the
    // protected recent window.
    for msg in session.messages.iter_mut() {
        // Only clear results in tool-role messages (that's where tool_result
        // blocks live — `ConversationMessage::tool_result` uses MessageRole::Tool).
        if msg.role != MessageRole::Tool {
            continue;
        }

        for block in &mut msg.blocks {
            let ContentBlock::ToolResult {
                tool_name,
                tool_use_id,
                output,
                ..
            } = block
            else {
                continue;
            };

            // Skip non-compactable tools.
            if !is_compactable(tool_name) {
                continue;
            }

            // Skip already-cleared results.
            if output == CLEARED_PLACEHOLDER {
                continue;
            }

            // Skip short results — not worth compacting.
            if output.len() < MIN_OUTPUT_LENGTH_FOR_CLEAR {
                continue;
            }

            // Skip results in the protected recent window UNLESS they're
            // dangerously large (emergency threshold). This prevents a single
            // huge output (e.g. 2MB grep) from exceeding the context window.
            if protect_set.contains(tool_use_id.as_str()) {
                if output.len() < EMERGENCY_CLEAR_THRESHOLD {
                    continue;
                }
                // Emergency clear: even protected results are cleared if too large
                eprintln!(
                    "[micro-compact: emergency clear of protected {} output ({} chars)]",
                    tool_name,
                    output.len()
                );
            }

            cleared_count += 1;
            chars_freed += output.len();
            cleared_tools.push(tool_name.clone());
            *output = CLEARED_PLACEHOLDER.to_string();
        }
    }

    let result = MicroCompactResult {
        cleared_count,
        chars_freed,
    };

    // Record the event to the GLM diagnostic log so we can verify it ran.
    if cleared_count > 0 {
        write_microcompact_diag(cleared_count, chars_freed, &cleared_tools, recent_ids.len());
    }

    result
}

/// Appends a micro-compact event record to `claw_glm_diag.log`, using the
/// same file/format as `write_glm_diag` so both event types appear in one
/// chronological log.
fn write_microcompact_diag(
    cleared_count: usize,
    chars_freed: usize,
    cleared_tools: &[String],
    protected_count: usize,
) {
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let tools_csv = cleared_tools.join(",");
    let record = format!(
        "\n==== claw_microcompact t={timestamp} cleared={cleared_count} chars_freed={chars_freed} protected={protected_count} ====\n\
         [cleared_tools] {tools_csv}\n"
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("claw_glm_diag.log")
    {
        let _ = f.write_all(record.as_bytes());
    }
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Returns true if the tool name is eligible for micro-compact.
fn is_compactable(tool_name: &str) -> bool {
    COMPACTABLE_TOOLS.iter().any(|&t| t == tool_name)
}

/// Collects the `tool_use_id`s of the most recent `keep` compactable tool
/// results, in **reverse** order of appearance (newest first).  These IDs are
/// the protected window — their content is kept verbatim so the model has
/// working context for multi-tool reasoning.
///
/// "Recent" is determined by message order in the session (later = newer),
/// which matches how the model sees the transcript.
fn collect_recent_compactable_ids(messages: &[ConversationMessage], keep: usize) -> Vec<String> {
    // Walk backwards collecting compactable tool_use_ids.
    let mut ids: Vec<String> = Vec::new();
    for msg in messages.iter().rev() {
        if msg.role != MessageRole::Tool {
            continue;
        }
        for block in &msg.blocks {
            if let ContentBlock::ToolResult {
                tool_name,
                tool_use_id,
                ..
            } = block
            {
                if is_compactable(tool_name) {
                    ids.push(tool_use_id.clone());
                    if ids.len() >= keep {
                        return ids;
                    }
                }
            }
        }
    }
    ids
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compactable_tools_include_read_file() {
        assert!(is_compactable("read_file"));
        assert!(is_compactable("grep_search"));
        assert!(is_compactable("bash"));
    }

    #[test]
    fn non_compactable_tools_excluded() {
        assert!(!is_compactable("TodoWrite"));
        assert!(!is_compactable("UnknownTool"));
    }

    #[test]
    fn collect_recent_ids_returns_all_when_few_results() {
        // No compactable tool results → empty vec
        let msgs = vec![
            ConversationMessage::user_text("hi"),
            ConversationMessage::assistant(vec![]),
        ];
        assert_eq!(collect_recent_compactable_ids(&msgs, 8), Vec::<String>::new());
    }

    #[test]
    fn collect_recent_ids_keeps_last_n_newest_first() {
        // 3 compactable tool results, keep=2 → returns the last 2, newest first.
        let msgs = vec![
            ConversationMessage::tool_result("t1", "read_file", "x", false),
            ConversationMessage::tool_result("t2", "read_file", "x", false),
            ConversationMessage::tool_result("t3", "read_file", "x", false),
        ];
        let ids = collect_recent_compactable_ids(&msgs, 2);
        assert_eq!(ids, vec!["t3".to_string(), "t2".to_string()]);
    }

    #[test]
    fn microcompact_clears_old_large_results_only() {
        // Two results: old+large (cleared), recent+large (kept).
        let large = "x".repeat(MIN_OUTPUT_LENGTH_FOR_CLEAR + 100);
        let mut session = Session::new();
        session.messages = vec![
            ConversationMessage::tool_result("old", "read_file", large.clone(), false),
            ConversationMessage::tool_result("new", "read_file", large.clone(), false),
        ];

        let result = microcompact_session(&mut session);
        // PROTECT_RECENT_TOOL_RESULTS >= 1 → "new" is kept, "old" is cleared.
        assert_eq!(result.cleared_count, 1);
        // The kept one is still verbatim; the cleared one is the placeholder.
        let cleared_output = match &session.messages[0].blocks[0] {
            ContentBlock::ToolResult { output, .. } => output.clone(),
            _ => String::new(),
        };
        assert_eq!(cleared_output, CLEARED_PLACEHOLDER);
        let kept_output = match &session.messages[1].blocks[0] {
            ContentBlock::ToolResult { output, .. } => output.clone(),
            _ => String::new(),
        };
        assert_eq!(kept_output, large);
    }

    #[test]
    fn microcompact_skips_short_results() {
        // Old but short → not cleared (below MIN_OUTPUT_LENGTH_FOR_CLEAR).
        let mut session = Session::new();
        session.messages = vec![ConversationMessage::tool_result(
            "old",
            "read_file",
            "tiny",
            false,
        )];
        let result = microcompact_session(&mut session);
        assert_eq!(result.cleared_count, 0);
    }
}

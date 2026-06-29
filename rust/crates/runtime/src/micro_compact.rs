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
const MIN_OUTPUT_LENGTH_FOR_CLEAR: usize = 500;

/// How many **recent** assistant turns (iterations) to protect from
/// micro-compact.  Results from the most recent N assistant messages
/// are always kept verbatim so the model still has fresh context.
/// Defaults to 2, matching claude-code's time-window heuristic.
const PROTECT_RECENT_ASSISTANT_TURNS: usize = 2;

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
pub fn microcompact_session(session: &mut Session) -> MicroCompactResult {
    let mut cleared_count = 0;
    let mut chars_freed = 0;

    // Find the index boundary: protect the last N assistant turns.
    let protect_from = protect_boundary(&session.messages);

    // Walk messages before the boundary and clear compactable tool results.
    for (msg_idx, msg) in session.messages.iter_mut().enumerate() {
        // Skip messages in the protected recent zone.
        if msg_idx >= protect_from {
            continue;
        }

        // Only clear results in user-role messages (that's where tool_result
        // blocks live in the Anthropic message format).
        if msg.role != MessageRole::User {
            continue;
        }

        for block in &mut msg.blocks {
            let ContentBlock::ToolResult {
                tool_name,
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

            cleared_count += 1;
            chars_freed += output.len();
            *output = CLEARED_PLACEHOLDER.to_string();
        }
    }

    MicroCompactResult {
        cleared_count,
        chars_freed,
    }
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Returns true if the tool name is eligible for micro-compact.
fn is_compactable(tool_name: &str) -> bool {
    COMPACTABLE_TOOLS.iter().any(|&t| t == tool_name)
}

/// Computes the message index from which tool results should be protected
/// (i.e. NOT cleared).  Messages at or after this index are "recent" and
/// their tool results are kept verbatim.
///
/// The boundary is determined by counting assistant-role messages from the
/// end and protecting the last `PROTECT_RECENT_ASSISTANT_TURNS` of them
/// plus all intervening user/tool messages.
fn protect_boundary(messages: &[ConversationMessage]) -> usize {
    if messages.len() <= PROTECT_RECENT_ASSISTANT_TURNS {
        return 0; // protect everything
    }

    // Walk backwards counting assistant turns.
    let mut assistant_count = 0;
    for i in (0..messages.len()).rev() {
        if messages[i].role == MessageRole::Assistant {
            assistant_count += 1;
            if assistant_count >= PROTECT_RECENT_ASSISTANT_TURNS {
                // Protect this assistant message and everything after it.
                return i;
            }
        }
    }

    // Fewer assistant turns than the threshold — protect everything.
    0
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
    fn protect_boundary_all_when_few_messages() {
        let msgs = vec![
            ConversationMessage::user_text("hi"),
            ConversationMessage::assistant_text("hello"),
        ];
        assert_eq!(protect_boundary(&msgs), 0);
    }

    #[test]
    fn protect_boundary_skips_recent_assistant_turns() {
        // 4 assistant turns → protect last 2 → boundary at index 4
        let msgs = vec![
            ConversationMessage::user_text("a"),       // 0
            ConversationMessage::assistant_text("b"),  // 1
            ConversationMessage::user_text("c"),       // 2
            ConversationMessage::assistant_text("d"),  // 3
            ConversationMessage::user_text("e"),       // 4
            ConversationMessage::assistant_text("f"),  // 5
            ConversationMessage::user_text("g"),       // 6
            ConversationMessage::assistant_text("h"),  // 7
        ];
        // PROTECT_RECENT_ASSISTANT_TURNS = 2
        // Walking backwards: assistant at 7 (count=1), assistant at 5 (count=2)
        // → protect from index 5
        assert_eq!(protect_boundary(&msgs), 5);
    }
}

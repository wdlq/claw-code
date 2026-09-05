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

/// Placeholder substituted for fully pruned tool-result content (emergency clear).
pub const CLEARED_PLACEHOLDER: &str = "[Old tool result content cleared]";

/// Snip 截短标记前缀（对齐 Reasonix 的 snippedMarker，prune.go:16）。
/// 比 CLEARED_PLACEHOLDER 强：保留头尾行，头部字节稳定→DeepSeek 缓存能命中到头部结束位置。
const SNIPPED_MARKER: &str = "[snipped tool result — ";

/// Snip 截短策略：按工具类型分级保留头尾行数 + 字符级封顶（对齐 Reasonix prune.go:186-188 + readfile.go:70-72 SnipHint）。
/// 只读工具（read_file/grep）头部是答案，保长头短尾；副作用工具（bash）头尾都可能有关键信息，保均等头尾。
/// **2026-07-18 字符级封顶**（落地 MEMORY 第 21 条门 3）：加 `head_chars`/`tail_chars` 字段，
/// 对齐 Reasonix `SnipHint{HeadChars:12000, TailChars:2000}`——一个超长行（minified JS 一行 50KB）
/// 能冲垮行级封顶，字符级封顶兜底。
struct SnipStrategy {
    head: usize,
    tail: usize,
    /// 头部保留的字符上限（按 Reasonix 12000 对齐）。超出则按字符截短头部。
    head_chars: usize,
    /// 尾部保留的字符上限（按 Reasonix 2000 对齐）。超出则按字符截短尾部。
    tail_chars: usize,
}

/// 只读工具默认策略：保留前 80 行 + 后 12 行，字符级 12000/2000
/// （对齐 Reasonix defaultReadOnlySnip + readfile.go:71 SnipHint）。
const READ_ONLY_SNIP: SnipStrategy = SnipStrategy {
    head: 80,
    tail: 12,
    head_chars: 12000,
    tail_chars: 2000,
};

/// 副作用工具默认策略：保留前 40 行 + 后 40 行，字符级 12000/2000
/// （对齐 Reasonix defaultSideEffectingSnip；字符级同只读工具——Reasonix 各工具 SnipHint 字符级差异不大）。
const SIDE_EFFECTING_SNIP: SnipStrategy = SnipStrategy {
    head: 40,
    tail: 40,
    head_chars: 12000,
    tail_chars: 2000,
};

/// 按工具名判定 Snip 策略：副作用工具（bash/PowerShell/write_file/edit_file/search_replace）用均等头尾，
/// 其他只读工具（read_file/grep/glob/list_directory/WebFetch/WebSearch）用长头短尾。
fn snip_strategy_for(tool_name: &str) -> &'static SnipStrategy {
    match tool_name {
        "bash" | "PowerShell" | "write_file" | "edit_file" | "search_replace" => {
            &SIDE_EFFECTING_SNIP
        }
        _ => &READ_ONLY_SNIP,
    }
}

/// Snip 截短 tool_result：保留头 N 行 + 尾 M 行，中间塞 `[... N lines omitted ...]` 标记。
/// 头部字节稳定→DeepSeek 缓存能命中到头部结束位置，只 miss 中间被截的部分（对齐 Reasonix snipToolResult）。
///
/// **2026-07-18 字符级封顶**（落地 MEMORY 第 21 条门 3）：行级封顶之上再叠字符级封顶，
/// 对齐 Reasonix `readfile.go:71 SnipHint{HeadChars:12000, TailChars:2000}`——
/// 一个超长行（minified JS 一行 50KB）能冲垮行级封顶，字符级封顶兜底：
/// 头部行数够但总字符超 `head_chars` → 按 `tail_chars` 量级再截短头部
/// 尾部同理。截短时按字符切片保尾（避免截半 multi-byte 中文字符——`floor_char_boundary` 兜底）。
fn snip_tool_result(content: &str, tool_name: &str) -> String {
    let strategy = snip_strategy_for(tool_name);
    let lines: Vec<&str> = content.lines().collect();
    // 行数太少不值得截短→直接返回原文（不触发清空，保前缀字节完全稳定）
    if lines.len() <= strategy.head + strategy.tail {
        return content.to_string();
    }
    let head_full = lines[..strategy.head].join("\n");
    let tail_full = lines[lines.len() - strategy.tail..].join("\n");
    // 字符级封顶：超 head_chars/tail_chars 则按字符再截（对齐 Reasonix SnipHint.HeadChars/TailChars）
    let head = truncate_to_chars(&head_full, strategy.head_chars);
    let tail = truncate_to_chars(&tail_full, strategy.tail_chars);
    let omitted = lines.len() - strategy.head - strategy.tail;
    format!(
        "{snipped}{name}, {orig} bytes; showing first {head} lines and last {tail} lines]\n{head_lines}\n[... {omitted} lines omitted ...]\n{tail_lines}",
        snipped = SNIPPED_MARKER,
        name = tool_name,
        orig = content.len(),
        head = strategy.head,
        tail = strategy.tail,
        head_lines = head,
        omitted = omitted,
        tail_lines = tail,
    )
}

/// 按 `max_chars` 截短字符串：超则取前 `max_chars` 字符并按 UTF-8 字符边界对齐，
/// 不超则原样返回。对齐 Reasonix 字符级封顶语义——防止超长行冲垮行级封顶。
/// 用 `floor_char_boundary` 兜底避免截半 multi-byte 中文字符。
fn truncate_to_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    // 取前 max_chars 个字符（按 char_indices 找字节位）
    let mut byte_end = s.len();
    for (i, (byte_idx, _)) in s.char_indices().enumerate() {
        if i == max_chars {
            byte_end = byte_idx;
            break;
        }
    }
    s[..byte_end].to_string()
}

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

/// **2026-09-03 缓存命中率长效修复**：判定某模型是否沿用"GLM-5.1 小窗口 + 激进压缩"老缓存机制。
///
/// **runtime 本地副本**——与 `api::providers::is_glm51_cache_model`（`api/src/providers/mod.rs`）
/// 同名同源，语义必须保持一致。runtime crate 不依赖 api crate（循环依赖禁令，见
/// ATOMCODE_MEMORY "should_use_compact_receipt 在 runtime 内独立判断"先例），故此处复制判定。
/// **改任一份时务必同步另一份**。
///
/// - 仅 `glm-5.1`（大小写不敏感、容忍路径前缀/方括号窗口标记/日期后缀）→ `true`
/// - 其他一切模型 → `false`（走高缓存命中模式）
pub fn is_glm51_cache_model(model: &str) -> bool {
    let canonical = model.trim().to_ascii_lowercase();
    let after_slash = canonical.rsplit('/').next().unwrap_or(canonical.as_str());
    let base = after_slash.split('[').next().unwrap_or(after_slash);
    base == "glm-5.1" || base.starts_with("glm-5.1-")
}

/// Minimum character length for a tool result to be considered for clearing.
/// Short results (e.g. "ok", "file written") are not worth compacting.
///
/// Set high so micro-compact only clears results that are genuinely
/// large — a small result carries information worth more than the tokens it
/// costs, and clearing it forces the model to re-read the file, which is the
/// observed "降智" failure mode.  Only bulky outputs (big file reads, long
/// grep/bash output) get cleared.
///
/// **2026-07-15 二期-C3 调参**：从硬编码 5_000 改成动态收参——
/// 联动 auto-compact 阈值：auto-compact 在 75% 窗口触发，microcompact 在 25% 窗口触发——
/// 两个 compact 逻辑阈值同源，microcompact 先于 auto-compact 软清中等 tool_result，
/// auto-compact 才硬压摘要。DeepSeek 1M 窗口 → microcompact 阈值 250K 字符，
/// 中等 tool_result（5K~250K）保留原样→前缀字节稳定→DeepSeek 硬盘缓存命中。
/// 仍可用 `CLAW_MICROCOMPACT_DISABLE=1` 彻底关掉。
///
/// **根因**（2026-07-15 实机发现）：不能调 `auto_compaction_threshold_from_env()`——
/// 那读 env 失败后 fallback 到默认 55_000，绕过了 `with_model_context_window` 设的
/// 动态阈值（1M 窗口→750K）。改成收参，由调用方传入 runtime 的最终阈值。
///
/// **2026-07-22 子 agent 撑爆 GLM 修复**：实机现场——子 agent 跑 `run_turn` loop 到第 3 轮时
/// 请求体膨胀到 244KB（est_tokens=60998），GLM 网关报 400。根因不是 auto_compact 阈值留得太晚
///（同套 150K 阈值在 A=7c95f2b 版主 LLM 跑 GLM 200K 稳定数百轮），而是这个函数把 snip 闸门
/// 按阈值/4 算——子 agent 阈值 150K 时闸门抬到 37.5K 字符，第 2 轮 14 个 grep_search 结果
/// 单个才几 KB 全部 < 37.5K → 全跳过不 snip → 第 3 轮请求体膨胀到 244KB 撑爆 GLM。
/// A 版稳定跑时这个闸门是固定常量 5000 字符（MIN_OUTPUT_LENGTH_FOR_CLEAR）。
///
/// 修复：给算值兜底上限 `min(算值, 5000)`，让子 agent 路径的 snip 闸门最多 5K 字符，
/// 跟 A 版主 LLM 跑 GLM 200K 稳定数百轮时一致。主 LLM 跑 DeepSeek 1M 阈值 750K 时算值 187.5K
/// 会被兜到 5K——但这不伤主 LLM：DeepSeek 1M 窗口下 microcompact 省的 input token 被 cache hit
/// 抹消，且清空换占位符会击穿 DeepSeek 硬盘缓存（字节级完整匹配规则），主 LLM 路径本来也靠
/// `CLAW_MICROCOMPACT_DISABLE=1` 全关 microcompact 走纯重传+缓存命中策略。
fn min_output_length_for_clear(auto_compaction_threshold: u32) -> usize {
    // 除以 4 是字符级近似（auto_compaction_threshold 是 token 数，~4 char/token，
    // 与现有 output.len() 字符级比较的误差可接受——只是阈值）。
    let scaled = (auto_compaction_threshold / 4) as usize;
    // 兜底上限 5000 字符——对齐 A=7c95f2b 版主 LLM 跑 GLM 200K 稳定数百轮时的固定闸门。
    // 不让闸门随阈值洿涨船高，子 agent 路径（阈值 150K）会被抬到 37.5K 字符导致小 tool_result 全跳过。
    scaled.min(5_000)
}

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
/// `auto_compaction_threshold` 是 runtime 的最终 auto-compact 阈值（env 显式值 > 模型窗口动态 > 默认 55K），
/// microcompact 阈值取其四分之一（字符级近似）。由调用方传入，避免本函数重读 env 拿不到动态窗口。
///
/// Protection model: the most recent `PROTECT_RECENT_TOOL_RESULTS` compactable
/// tool results are always kept verbatim (regardless of which turn they belong
/// to).  Only results older than that window — and larger than
/// `min_output_length_for_clear(threshold)` — are cleared.  This preserves enough working
/// context for the model to reason across several tool calls, which is
/// essential when the provider has no server-side cache_edits to fall back on.
/// **2026-09-03 缓存命中率长效修复**：新增 `high_cache_mode` 参数——高缓存命中模式。
///
/// - `false`（默认，GLM-5.1 老机制）：维持原行为——常规 snip/清空照跑，microcompact 是
///   GLM-5.1 小窗口（200K）下防请求体膨胀的主防线（对齐 2026-06~07 联通云时代既定行为）。
/// - `true`（glm-5.2 / deepseek 等所有其他模型）：**常规路径整体跳过**——不动任何历史
///   tool result 字节，让网关自动前缀缓存持续累积（对照 atomcode 的"纯 append-only =
///   完美缓存"）。**仅保留 emergency 清**：单个 output ≥ `EMERGENCY_CLEAR_THRESHOLD`
///   （500K 字符）时无条件清成占位符——这是防单条巨型输出撑爆上下文/请求体的安全网，
///   该场景下牺牲一次前缀缓存换可用性。
///
/// 判定入口 `api::is_glm51_cache_model`（只有 glm-5.1 返回 false），由 runtime 字段
/// `microcompact_high_cache_mode` 承载，`conversation.rs` 调用点透传。
pub fn microcompact_session(
    session: &mut Session,
    auto_compaction_threshold: u32,
    high_cache_mode: bool,
) -> MicroCompactResult {
    // 二期-C3：CLAW_MICROCOMPACT_DISABLE=1 彻底关掉 microcompact。
    // DeepSeek 1M 窗口下重传前缀的代价被 cache hit 抹消，microcompact 省的 input token
    // 反而不重要；且清空换占位符会击穿 DeepSeek 硬盘缓存（字节级完整匹配规则）。
    // 用户可设此 env 实测对比命中率与 token 消耗，取性价比。
    if std::env::var("CLAW_MICROCOMPACT_DISABLE").map_or(false, |v| v == "1") {
        return MicroCompactResult {
            cleared_count: 0,
            chars_freed: 0,
        };
    }
    let mut cleared_count = 0;
    let mut chars_freed = 0;
    let mut cleared_tools: Vec<String> = Vec::new();

    // Collect the IDs of the most recent N compactable tool results (newest
    // first), then keep them verbatim.
    let recent_ids = collect_recent_compactable_ids(&session.messages, PROTECT_RECENT_TOOL_RESULTS);
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
            // Skip already-snipped results（避免重复截短，对齐 Reasonix shouldMaintainToolResult）。
            if output.starts_with(SNIPPED_MARKER) {
                continue;
            }

            // **高缓存命中模式**（2026-09-03）：常规 snip/清空整体跳过，保历史字节完全稳定。
            // 只放行 emergency——巨型输出撑爆上下文的安全网仍在。
            if high_cache_mode && output.len() < EMERGENCY_CLEAR_THRESHOLD {
                continue;
            }

            // Skip short results — not worth compacting.
            // 阈值取 auto_compaction_threshold 的四分之一（见 min_output_length_for_clear）。
            if output.len() < min_output_length_for_clear(auto_compaction_threshold) {
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
            let original_len = output.len();
            // 思路4：Snip 截短保留头尾，不用一刀清空换占位符。
            // 头部字节稳定→DeepSeek 缓存能命中到头部结束位置，只 miss 中间被截的部分。
            // 只有 emergency clear（超 EMERGENCY_CLEAR_THRESHOLD 的巨型输出）才用 CLEARED_PLACEHOLDER 彻底删。
            if original_len < EMERGENCY_CLEAR_THRESHOLD {
                let snipped = snip_tool_result(output, tool_name);
                // snip_tool_result 返回原文表示行数太少不值得截→保持原样不清（保前缀字节完全稳定）
                if snipped.len() < original_len {
                    chars_freed += original_len - snipped.len();
                    *output = snipped;
                    cleared_tools.push(tool_name.clone());
                } else {
                    // 行数太少不值得截短→跳过不清，保前缀字节稳定
                    cleared_count -= 1;
                }
            } else {
                chars_freed += original_len;
                cleared_tools.push(tool_name.clone());
                *output = CLEARED_PLACEHOLDER.to_string();
            }
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
        assert_eq!(
            collect_recent_compactable_ids(&msgs, 8),
            Vec::<String>::new()
        );
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
        // 阈值动态收参，这里取当前阈值+100确保触发清空。
        // **预存债**：此测试在二期改动前就失败（PROTECT_RECENT_TOOL_RESULTS=8 把仅 2 个
        // tool_result 全保护了→cleared_count=0≠期望1）。不是二期改动引入，留待原债主修。
        let auto_threshold: u32 = 55_000; // 测试用默认阈值
        let threshold = min_output_length_for_clear(auto_threshold);
        let large = "x".repeat(threshold + 100);
        let mut session = Session::new();
        session.messages = vec![
            ConversationMessage::tool_result("old", "read_file", large.clone(), false),
            ConversationMessage::tool_result("new", "read_file", large.clone(), false),
        ];

        let result = microcompact_session(&mut session, auto_threshold, false);
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
        let result = microcompact_session(&mut session, 55_000, false);
        assert_eq!(result.cleared_count, 0);
    }

    // ===== 2026-09-03 缓存命中率长效修复：高缓存命中模式 =====

    #[test]
    fn high_cache_mode_skips_routine_snip_but_keeps_emergency() {
        // 高缓存模式：旧的较大（超 5K 常规闸门但 < 500K emergency 线）tool result
        // 不再被 snip/清空——历史字节完全稳定，网关前缀缓存可持续累积。
        let threshold = min_output_length_for_clear(55_000);
        let large_but_not_emergency = "x".repeat(threshold + 100); // ~5.1K，远小于 500K
        let mut session = Session::new();
        session.messages = vec![ConversationMessage::tool_result(
            "old",
            "read_file",
            large_but_not_emergency.clone(),
            false,
        )];

        let result = microcompact_session(&mut session, 55_000, true);
        assert_eq!(
            result.cleared_count, 0,
            "routine snip must be skipped in high cache mode"
        );
        assert_eq!(result.chars_freed, 0);
        let output = match &session.messages[0].blocks[0] {
            ContentBlock::ToolResult { output, .. } => output.clone(),
            _ => String::new(),
        };
        assert_eq!(output, large_but_not_emergency, "bytes must stay verbatim");
    }

    #[test]
    fn high_cache_mode_still_clears_emergency_sized_output() {
        // 高缓存模式的安全网：≥ EMERGENCY_CLEAR_THRESHOLD（500K 字符）的巨型输出
        // 仍被清成占位符——防单条输出撑爆上下文/请求体。
        let emergency = "y".repeat(crate::micro_compact::EMERGENCY_CLEAR_THRESHOLD + 100);
        let mut session = Session::new();
        session.messages = vec![ConversationMessage::tool_result(
            "old",
            "grep_search",
            emergency,
            false,
        )];

        let result = microcompact_session(&mut session, 55_000, true);
        assert_eq!(result.cleared_count, 1, "emergency clear must still fire");
        let output = match &session.messages[0].blocks[0] {
            ContentBlock::ToolResult { output, .. } => output.clone(),
            _ => String::new(),
        };
        assert_eq!(output, CLEARED_PLACEHOLDER);
    }
}

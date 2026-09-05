# compact 修改计划：对齐 claude-code 的 auto-compact 机制（2026-09-03 调研结论落地）

> **写给后续接手本修改的 AI/人**。动手前先读 `ATOMCODE_MEMORY.md` 的"2026-09-03 缓存命中率长效修复"章节
> （含追加段）——本计划建立在那次修复之上，**不得破坏其已落地的机制**（见 §6"不许动的清单"）。
> 参考实现：`E:\Claude Code\ClaudeCode2.1.88开源版\claude-code-source-code`
> - `src/services/compact/autoCompact.ts`（阈值公式 + 熔断器）
> - `src/utils/tokens.ts:226-261`（回执锚点估算 `tokenCountWithEstimation`）
> - `src/query.ts:453`（单触发点调用位置）

---

## 1. 背景与动机

### 1.1 2026-09-03 已确认的根因链（日志铁证）

- `claw_glm_diag.log`（38.5MB，3 小时窗口，372 请求）：main 命中率 69.6%、subagent 79.9%，
  **cache_read 峰值只有 42K/50K**；3 小时 **31 次 auto_compact**，threshold 全是 55000。
- 根因：网关 model 名（`GLM-5.2`、`DeepSeek-V4-Flash-0731`）与 `model_token_limit` 表不匹配 →
  动态阈值静默回落 55K → 频繁 compact 击穿前缀缓存。
- 已落地修复（api 归一化 + glm-5.2 条目 + 前缀兜底 + glm-5.1-only 老机制 + 高缓存模式 +
  cache_control 禁用 + glm-5.1 子 agent 160K）。**本计划不改这些**，只解决调研发现的剩余问题。

### 1.2 本计划要解决的三个问题（2026-09-03 调研 claude-code 后确认）

| # | 问题 | 位置 | 严重度 |
|---|---|---|---|
| P1 | **`maybe_auto_compact` 用 `cumulative_usage().input_tokens` 判断触发——`cumulative` 是跨 turn 累加值，不是当前上下文大小**。只增不减：一旦累计破阈值，之后每轮都满足条件，会 2-3 轮压一次停不下来；活跃会话也会被"历史累计"误触发 | `conversation.rs:833`（`maybe_auto_compact`）+ `usage.rs:194`（`record()` 里 `cumulative.input_tokens +=`） | ★★★ 最高 |
| P2 | **双触发点**：请求前 pre-flight（`session_needs_pre_flight_compact`，纯 char 粗估）+ turn 结束后（`conversation.rs:770` 直调 `maybe_auto_compact`）。两套估算口径不一致，turn 后触发多一次前缀击穿风险 | `conversation.rs:882`（pre-flight）+ `:770`（turn 后） | ★★ |
| P3 | 阈值是逐模型手定常数（160K/702K/90K），无公式；env 覆盖可设任意值（用户 `.claw.json` 里 GLM-5.1 时代旧值 `WINDOW=131000` 仍会把主 lane 阈值顶到 131K）；proactive compact 无失败熔断 | `main.rs:8756-8781`、`tools/src/lib.rs:build_agent_runtime`、`conversation.rs:with_model_context_window` | ★★ |

### 1.3 claude-code 的做法（对标基线）

```
触发时机   单点：query 循环每轮发请求前（query.ts:453）
阈值公式   有效窗口 = contextWindow − min(maxOutputTokens, 20K 摘要预留)
           触发阈值 = 有效窗口 − 13K 缓冲          → 200K 窗口 = 167K（83.5%）
           env 覆盖只能提前不能拖后（Math.min 封顶）
token 估算 回执锚点：从消息末尾向前找最近一条带真实 usage 的 assistant 消息，
           tokenCount = usage锚点(input + cache_creation + cache_read + output)   ← 服务端报的全量上下文
                      + 粗估(锚点之后的尾部消息)                                    ← 只粗估新增尾巴
           找不到锚点才全量粗估兜底；snip 省掉的量从估算中扣除（snipTokensFreed）
熔断器     连续 3 次 autocompact 失败停止重试（Anthropic 实测 250K 次 API 调用/天的浪费教训）
```

对 200K 级 LLM：claude-code 触发在 167K，比 claw 现行 glm-5.1 子 agent 的 160K 晚 7K——
两者同量级，**用户已定的 160K 继续保留**（见 §4 Step 3）。

---

## 2. 目标（改完要达成什么）

- **G1 估算正确**：触发判断基于"最近一次真实回执 + 尾部增量粗估"，对齐 claude-code
  `tokenCountWithEstimation`；无回执时回退现有 CJK 感知粗估。彻底消灭 cumulative 误判。
- **G2 单触发点**：全 runtime 只剩"发请求前"一个 compact 检查点；删除 turn 后的独立触发路径。
- **G3 阈值公式化且逐模型自适应**：`窗口 − min(max_output, 20K) − 13K`，下限保护 55K；
  **glm-5.1 子 agent 保持 160K 显式值**（用户决策 2026-09-03，不改）；glm-5.1 主 lane 保持
  env 可覆盖但加 min 封顶（G4）。
- **G4 env 覆盖只能提前、不能拖后**：百分比/直接阈值 env 一律 `min(env值, 公式默认值)`。
- **G5 熔断器**：连续 3 次 proactive compact 无效（`removed_message_count == 0` 或压后仍超）即停，
  交给 over_size_400 reactive 路径兜底；成功后清零。
- **G6（可选 P2，最后做）**：snip/microcompact 释放的量从触发估算中扣除
  （对齐 claude-code `snipTokensFreed`）。
- **硬约束**：deepseek 全系、glm-5.2 及其他 glm 型号的**前缀缓存机制原样保留**——
  高缓存模式（microcompact 仅 emergency 清）、strict 大阈值、cache_control 禁用，
  三者一个都不能回退（见 §6）。

---

## 3. 现状盘点（改前必读的代码事实）

全部行号以 2026-09-03 修复后的工作区为准，动手前先 `grep` 复核。

### 3.1 runtime/src/conversation.rs

| 位置 | 现状 |
|---|---|
| `:833` `maybe_auto_compact` | `let input_tokens = self.usage_tracker.cumulative_usage().input_tokens;` ← **P1 bug 所在**。`input_tokens == 0` 时回退 `estimate_tokens_mixed(byte_count)`（CJK 感知，:778 附近有实现） |
| `:857-867` | `estimated_tokens < threshold → None`；否则 `compact_session(CompactionConfig { max_estimated_tokens: 0, ..default })`——注意 `max_estimated_tokens: 0` 意为"压到最小" |
| `:770` turn 后触发点 | `let auto_compaction = self.maybe_auto_compact();` + `write_auto_compact_diag(...)` ← **P2 要删的第二个触发点**（diag 事件发射要保留在幸存的触发点里） |
| `:515` 附近请求前触发点 | `if self.session_needs_pre_flight_compact() { if let Some(event) = self.maybe_auto_compact() {...} }` ← 幸存触发点（改成新估算函数） |
| `:882` `session_needs_pre_flight_compact` | char 级粗估 + `SYSTEM_OVERHEAD_TOKENS=15_000` + CJK 感知 `estimate_tokens_mixed`——**锚点 fallback 要复用这里的估算件** |
| `:270` `with_model_context_window` | env 显式阈值优先，否则 `窗口×75%`（下限 55K）← **G4 要加 min 封顶** |
| `:305` `with_model_context_window_strict` | `(窗口 − max_output)×75%`，不读 env ← glm-5.2/deepseek 子 lane 在用 |
| `:187` 字段 | `auto_compaction_input_tokens_threshold: u32`；`:194` `microcompact_high_cache_mode`（勿动） |
| `:588` over_size_400 降级 | `compact_session(max_estimated_tokens=0)` 强压后重试 ≤3 次 ← reactive 兜底，**保留不动**，熔断器只管 proactive |

### 3.2 runtime/src/usage.rs

- `record()`（:192-199）：`cumulative.input_tokens += usage.input_tokens` 等——**cumulative 是计费/统计语义，
  保留 UsageTracker 不动**（cost 展示还用它），只是 compact 判断不再读它。
- `current_turn_usage()`（:202）返回 `latest_turn`——**turn 内最后一次 API 调用的回执**，
  是现成锚点候选之一；但更稳的锚点要从 session 消息里走（见 §4 Step 1）。

### 3.3 runtime/src/compact.rs

- `CompactionConfig::default()`：`preserve_recent_messages=4`、`max_estimated_tokens=10_000`。
- `should_compact`（:41）：`可压缩消息数 > preserve_recent && 估算 tokens ≥ max_estimated_tokens`。
- `compact_session`（:96）：`should_compact` 不过 → 返回 `removed_message_count=0`（熔断器要识别这个）。
- `estimate_session_tokens`（:35）+ `estimate_message_tokens`——尾部粗估可复用。

### 3.4 阈值来源矩阵（改前）

| lane | model | 现行阈值 | 来源 |
|---|---|---|---|
| 子 agent | glm-5.1 | **160 000** 固定 | `tools/src/lib.rs::subagent_auto_compact_threshold`（2026-09-03 用户决策：压缩后内容 <40K 为宜） |
| 子 agent | 其他（glm-5.2/deepseek/claude…） | `(窗口−max_output)×75%`，下限 55K | `with_model_context_window_strict`（不读 env） |
| 主 lane | glm-5.1 | env 显式 > `窗口×75%`（下限 55K） | `with_model_context_window`（读 env） |
| 主 lane | 其他 | 同上（查表命中才设，未命中 55K 兜底） | 同上 |

### 3.5 claude-code 参考实现关键行

- `autoCompact.ts:30` `MAX_OUTPUT_TOKENS_FOR_SUMMARY=20_000`；`:62` `AUTOCOMPACT_BUFFER_TOKENS=13_000`；
  `:70` `MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES=3`；`:72-91` `getAutoCompactThreshold`；
  `:160-239` `shouldAutoCompact`。
- `tokens.ts:226-261` `tokenCountWithEstimation`（锚点向前走，含 split-response 同 id 归并）；
  `:46-53` `getTokenCountFromUsage`（锚点公式含 output_tokens）。
- `query.ts:453` 单触发点。

---

## 4. 分步改法

> 顺序执行，每步独立可编译、可测试、可回滚。Step 1+2 是核心（P1/P2），Step 3-5 是增强。

### Step 1 修估算：回执锚点 + 尾部粗估（解决 P1）

**新函数**（建议放 `runtime/src/compact.rs`，或 `conversation.rs` 伴随模块，需能被两 lane 复用）：

```rust
/// 对齐 claude-code tokenCountWithEstimation：
/// 锚点 = 会话中最近一条带 usage 的 assistant 消息的回执全量
///        （input + cache_creation + cache_read + output——服务端报的"当时上下文总量"，
///          天然包含 system prompt + tools 定义，不需要客户端拼）；
/// 估算  = 锚点 + 锚点之后各消息的 estimate_message_tokens 粗估和。
/// 找不到锚点（首轮 / 网关不回 usage）→ 全量粗估兜底：
///        estimate_session_tokens(session) + SYSTEM_OVERHEAD_TOKENS(15K)。
pub fn estimate_context_tokens(session: &Session) -> usize
```

实现要点：
1. **从 `session.messages` 末尾向前找锚点**（不要用 `UsageTracker`——turn 内多轮 API 调用时
   tracker 只有最后一轮回执，且 cumulative 语义错误）。claw 的 `ConversationMessage` 带
   `usage: Option<TokenUsage>`（`UsageTracker::from_session` 已在消费它），数据在会话里就有。
2. 锚点公式对齐 `getTokenCountFromUsage`：`input_tokens + cache_creation_input_tokens +
   cache_read_input_tokens + output_tokens`。
3. **GLM 网关兼容**：`conversation.rs:830` 注释已言明部分网关不回 `input_tokens`——usage 缺失/全零的
   消息不算锚点，继续向前走；整条会话找不到就走兜底粗估。**动手前先扒一次日志确认目标网关回执字段**：
   `grep "claw_cache_diag" claw_glm_diag.log` 里 `input=`/`cache_read=` 有值即锚点可用
   （8-28 日志里 scnet 网关 main/sub 两条 lane 都有值）。
4. 尾部粗估逐消息调 `estimate_message_tokens`（compact.rs 现有，CJK 已适配）；
   不要图省事用 bytes/4。
5. `SYSTEM_OVERHEAD_TOKENS` 只加在兜底路径——锚点路径服务端已含系统开销，再加就双算。

**替换点**：
- `maybe_auto_compact`（:829-858）里的估算段整体换成 `estimate_context_tokens(&self.session)`，
  **删除** `cumulative_usage()` 读取与 `input_tokens==0` 分支。
- `session_needs_pre_flight_compact`（:882）改成调 `estimate_context_tokens` 后与阈值比较
  （或整个删除、由 Step 2 的新检查函数替代——推荐后者，见下）。

**注意**：`UsageTracker` 本身一行不改（计费统计仍需要 cumulative）。

### Step 2 收敛触发点：只留"请求前"一次（解决 P2）

1. 新增私有方法 `fn auto_compact_if_needed(&mut self) -> Option<AutoCompactionEvent>`：
   `estimate_context_tokens ≥ threshold` 时调 `compact_session`（沿用 `max_estimated_tokens: 0`）、
   更新 session、发 `write_auto_compact_diag`、**接入熔断器计数（Step 5）**。
2. **请求前检查点**（现 :515 pre-flight 处）：改为调 `auto_compact_if_needed`。
   删除 `session_needs_pre_flight_compact`（其 CJK 估算件被 Step 1 复用后，函数本体不再需要）。
3. **删除 turn 后触发点**（:770-776）：`let auto_compaction = self.maybe_auto_compact();` 整段移除。
   `TurnSummary.auto_compaction` 字段改为记录本轮请求前那次 compact 的 event（若有），
   保持 CLI 渲染与 jsonl 兼容——**先 grep `auto_compaction` 的全部消费方再动**。
4. `maybe_auto_compact`（pub）若还有外部调用方，保留为薄壳转发 `auto_compact_if_needed`；
   没有就删。动手前 `grep -rn "maybe_auto_compact" rust/`。

### Step 3 阈值公式化 + 保留既有决策（解决 P3 前半）

新增共享 helper（建议 `runtime/src/conversation.rs`，pub 供 main.rs/tools 使用）：

```rust
/// 对齐 claude-code getAutoCompactThreshold：
/// (窗口 − min(max_output, 20K 摘要预留) − 13K 缓冲)，下限 55K。
pub fn autocompact_threshold_formula(context_window_tokens: u32, max_output_tokens: u32) -> u32
```

各 lane 接线：

| lane | model | 改法 |
|---|---|---|
| 子 agent | glm-5.1 | **不变**：`subagent_auto_compact_threshold` 继续返回 `Some(160_000)`（用户决策优先于公式；公式给 167K，差异已向用户说明过）。该函数已有 2 个测试，别删 |
| 子 agent | 其他 | `with_model_context_window_strict` 保留，但阈值算法从 `(窗口−max_output)×75%` 换成 `autocompact_threshold_formula`（glm-5.2 1M → 967K，deepseek-v4-flash 128K → 107K）。**这是行为变更点**：数值普遍比 75% 版晚触发，对前缀缓存更友好；§6 的机制（high_cache_mode/strict 不读 env）不变 |
| 主 lane | glm-5.1 | `with_model_context_window` 保留（env 语义见 Step 4）；env 未设时动态值从 `窗口×75%` 换成 `autocompact_threshold_formula(窗口, max_tokens_for_model)` |
| 主 lane | 其他 | 同 glm-5.1 分支处理（env min 封顶后动态值用公式） |

`conversation.rs` 里三个既有测试 `with_model_context_window_strict_*`（:1990-2059 附近）断言的是
75% 公式的具体数值——**换公式必须同步改这些断言**，并保留"strict 不读 env"的对照测试
（注意那组测试要 `--test-threads=1`，env 全局污染坑 MEMORY 第 25 条有记）。

### Step 4 env 覆盖加 min 封顶（解决 P3 后半）

`with_model_context_window`（:270-282）改造，对齐 claude-code 语义：

- `CLAUDE_CODE_AUTO_COMPACT_WINDOW`：从"替代窗口"改为"**封顶**有效窗口"：
  `effective_window = min(模型窗口, env值)`，再进公式。
- `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`（若要新增对齐）与既有
  `CLAUDE_CODE_AUTO_COMPACT_PCT_OVERRIDE`/`CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS`：
  最终阈值一律 `min(env算出的阈值, 公式默认阈值)`——**只能提前，不能拖后**。
- 保留一个显式逃生口：`CLAUDE_AUTOCOMPACT_THRESHOLD_UNCAPPED=1` 时跳过封顶
  （调试用，默认不设）。
- **提示用户**：`.claw.json` 里 `CLAUDE_CODE_AUTO_COMPACT_WINDOW=131000` 是 GLM-5.1 200K 时代旧值，
  封顶语义下会把 glm-5.1 有效窗口压到 131K→阈值 ~98K，频繁 compact。建议用户删掉该 env——
  改动落地后在总结里明确写这一条。

### Step 5 熔断器（G5）

1. `ConversationRuntime` 加字段 `auto_compact_consecutive_failures: u32`（默认 0）。
2. `auto_compact_if_needed` 内：
   - 触发 compact 后 `removed_message_count == 0`（压不动）或压后 `estimate_context_tokens` 仍 ≥ 阈值
     → `failures += 1`，`eprintln!("[auto-compact: ineffective ×N/3]")`；
   - compact 生效（removed > 0 且压后低于阈值）→ `failures = 0`；
   - `failures >= 3` → **跳过本次 proactive compact**，打日志说明交给 over_size_400 reactive 路径，
     直接返回 None。
3. 参照 `autoCompact.ts:67-70` 的注释写清楚动机（250K 次/天浪费的教训），后人不会误删。
4. 新增测试：连续 3 次压不动后第 4 次不再尝试；成功一次后计数清零恢复尝试。

### Step 6（可选 P2）snip 联动

`microcompact_session` 已返回 `MicroCompactResult { cleared_count, chars_freed }`（runtime/micro_compact.rs）。
把 `chars_freed` 折算 tokens（除以现有 CJK 估算比率，或简单 `estimate_tokens_mixed(chars_freed)`）
从本轮 `estimate_context_tokens` 结果中扣除。仅当 Step 1-5 稳定后再做；
先在 `auto_compact_if_needed` 签名里留 `snip_tokens_freed: usize = 0` 参数位。

---

## 5. 测试与真机验证

### 5.1 单元测试（新增/必改清单）

| 测试 | 断言 |
|---|---|
| `estimate_context_tokens_uses_latest_receipt_as_anchor` | 消息带 usage（input=1000, cache_read=4096, output=200）+ 尾部 2 条粗估消息 → 结果 = 5296 + 尾部粗估；**不随更早消息的 usage 变化** |
| `estimate_context_tokens_falls_back_to_char_estimate_without_usage` | 全部消息无 usage → `estimate_session_tokens + 15K` |
| `estimate_context_tokens_skips_zero_usage_messages` | 网关回 usage 全零（GLM 兼容形态）→ 不当锚点 |
| `autocompact_threshold_formula_*` | 200K/64K → 167K；200K/128K → 167K（min封顶20K）；1M/64K → 967K；128K/8K → 107K；下限 55K 生效（如 60K 窗口） |
| `with_model_context_window_env_is_capped_not_replacement` | env WINDOW=131000 + 模型 1M → 阈值按 131K 有效窗口算（封顶），**不是**直接拿 env 当阈值 |
| `auto_compact_circuit_breaker_stops_after_3_failures` / `_resets_on_success` | 见 Step 5 |
| 既有必改 | `with_model_context_window_strict_*` 3 条（新公式数值）；`with_cache_mode_for_model_only_glm51_keeps_legacy_compaction`（必须原样通过）；`subagent_threshold_tests` 2 条（160K 不变）；api 侧 `model_token_limit_*`/`is_glm51_cache_model_*`/cache_control 3 条（**不许被波及**） |

### 5.2 真机验证（重编 `cargo build --release` 替换 claw.exe 后）

```bash
LOG="E:/NW工程/资料库/html/claw_glm_diag.log"
# ① threshold 指纹：glm-5.1 子 agent=160000；glm-5.2≈967000；不应再出现 55000（除非未知模型兜底）
grep -E "^==== claw_auto_compact" "$LOG" | grep -oE "threshold=[0-9]+" | sort | uniq -c
# ② compact 频率：同会话内不应出现 2-3 轮一次的连环 compact（cumulative bug 已死的证据）
grep -E "^==== claw_auto_compact" "$LOG" | wc -l     # 对比修复前 3 小时 31 次
# ③ 缓存天花板：cache_read 峰值应能远超旧的 50K（glm-5.2 期望几十万级）
grep -oE "cache_read=[0-9]+" "$LOG" | sort -t= -k2 -n | tail -5
# ④ 熔断器：出现 "ineffective" 字样且次数 ≤3 后停止
grep -c "auto-compact: ineffective" "$LOG"
```

### 5.3 回归护栏（跑完必须全绿或仅剩 MEMORY 已记预存债）

```bash
cd rust && cargo check --workspace
cargo test -p api --lib          # 164 passed 基线（cache_control 14 条含 disabled 用例）
cargo test -p runtime --lib      # 新增测试过；microcompact_clears_old_large_results_only 是预存债可 FAIL
cargo test -p tools --lib subagent_threshold_tests
bash scripts/fmt.sh --check && cargo clippy -p api -p runtime -p tools 2>&1 | tail -5   # 零新增
```

---

## 6. 不许动的清单（前缀缓存机制保全约束）

用户要求：**deepseek 全系、glm-5.2 及其他 glm 型号的前缀缓存机制必须原样保留**。以下机制
2026-09-03 已落地并有测试锁定，本计划所有 Step 都不得触碰其行为：

| 机制 | 位置 | 锁定测试 |
|---|---|---|
| `is_glm51_cache_model`：仅 glm-5.1 true | `api/src/providers/mod.rs` + `runtime/src/micro_compact.rs`（**双副本，改任一必须同步另一份**） | api `is_glm51_cache_model_matches_only_glm51`；runtime builder 测试 |
| `model_registry_key` 归一化 + glm-5.2 条目 + 前缀兜底 | `api/src/providers/mod.rs`（注意 `glm-5.2*` 前缀判断必须先于 `glm-5.*`） | `model_token_limit_normalizes_gateway_names` 等 3 条 |
| 高缓存模式：microcompact 常规跳过、仅 emergency（≥500K 字符）清 | `runtime/src/micro_compact.rs::microcompact_session(high_cache_mode)`；`conversation.rs` 字段 `microcompact_high_cache_mode` + `with_cache_mode_for_model` | `high_cache_mode_skips_routine_snip_but_keeps_emergency` / `..._still_clears_emergency_sized_output` |
| cache_control 禁用（非 glm-5.1 → `CacheConfig::disabled()`） | `api/src/cache_control.rs` + `main.rs` AnthropicRuntimeClient 构造（**E0382 坑：cache_config 计算必须在 `Ok(Self{...})` 之前**，`model,` 字段会 move）+ `tools/src/lib.rs` new_with_resolved（用 `resolved.model` 判定） | `cache_config_disabled_is_noop_for_both_marker_kinds` |
| over_size_400 reactive 降级重试（≤3 次强压后重试） | `conversation.rs:588` 附近 | 熔断器（Step 5）只管 proactive，**不得削减 reactive 重试次数** |
| 主 lane env 覆盖能力（glm-5.1 场景） | `with_model_context_window` | Step 4 只改封顶语义，不删除 env 读取 |

**per-lane 判定范式**（multiprovider 双端各自正确）：主 lane 用主 LLM model 名、子 lane 用
`resolved.model`，两条 lane 各自独立走 §4 Step 3 的接线表——不要引入任何全局共享的 model 状态
（thread-local `SUBAGENT_MODEL` 只服务 compact_receipt，别复用到阈值上）。

---

## 7. 风险与回滚

| 风险 | 缓解 |
|---|---|
| 目标网关回执不带 usage / 字段名不同（DeepSeek 原生是 `prompt_cache_hit_tokens` 等） | 锚点找不到自动走兜底粗估，行为不差于现状；落地前先扒 claw_cache_diag 确认（§4 Step 1 要点 3） |
| 阈值公式切换后 compact 更晚触发 → 极端会话更早撞 over_size | reactive 降级路径仍在（≤3 次）；且公式已含 20K+13K 双缓冲 |
| env 老用户（WINDOW=131000）阈值被压更低 | min 封顶只能"更早不能更晚"，方向安全；总结里建议清 env + 提供逃生口 env |
| 删 turn 后触发点影响 TurnSummary/CLI 渲染 | 动手前 grep `auto_compaction`/`maybe_auto_compact` 全部消费方；TurnSummary 字段保留、来源改为请求前 event |
| cumulative 语义还有其他消费方误用 | 本次只改 compact 判断；顺手 `grep -rn "cumulative_usage"` 记录其他消费点（计费展示是合法用途），不扩改 |

**回滚单元**：Step 1+2 一个 commit（P1/P2 核心），Step 3+4 一个 commit（阈值/env），Step 5 一个
commit（熔断），Step 6 独立。任一 revert 不影响其余步骤编译。

---

## 8. 验收标准（给 review 者的一句话清单）

1. 日志里 auto_compact 的 threshold 不再出现 55000（glm-5.1 子 agent=160000、glm-5.2≈967000）。
2. 同一子 agent 会话不再出现"每 2-3 轮一次"的连环 compact。
3. cache_read 峰值显著突破旧的 42-50K 天花板。
4. `is_glm51_cache_model`/高缓存模式/cache_control 禁用三套机制的行为矩阵与 2026-09-03 完全一致
   （§6 测试全绿）。
5. `cumulative_usage()` 不再出现在任何 compact 判断路径中（grep 可验）。

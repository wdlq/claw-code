use std::cmp::Reverse;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use glob::Pattern;
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use walkdir::{DirEntry, WalkDir};

/// Maximum file size that can be read (10 MB).
const MAX_READ_SIZE: u64 = 10 * 1024 * 1024;

/// Maximum file size that can be written (10 MB).
const MAX_WRITE_SIZE: usize = 10 * 1024 * 1024;

/// Maximum characters a grep content result may have before being
/// persisted to disk.  When the output exceeds this threshold the
/// content is written to `.claw/persisted/grep_results/` and the
/// `content` field is replaced with a file-reference string so the
/// LLM can read it on demand.
const DEFAULT_MAX_RESULT_SIZE_CHARS: usize = 50_000;

const GLOB_SEARCH_IGNORED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    ".build",
    "target",
    "dist",
    "coverage",
    // Claw session/cache dirs (contain huge JSONL files with embedded file contents;
    // searching them bloats the context window to millions of tokens)
    ".claw",
    // Python virtual-envs and caches (can contain tens of thousands of files)
    ".venv",
    "venv",
    "__pycache__",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".cache",
    // Rust build artefacts
    "cargo-target",
    // Java / Gradle
    ".gradle",
    ".mvn",
];

/// Check whether a file appears to contain binary content by examining
/// the first chunk for NUL bytes.
fn is_binary_file(path: &Path) -> io::Result<bool> {
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut buffer = [0u8; 8192];
    let bytes_read = file.read(&mut buffer)?;
    Ok(buffer[..bytes_read].contains(&0))
}

/// Validate that a resolved path stays within the given workspace root.
/// Returns the canonical path on success, or an error if the path escapes
/// the workspace boundary (e.g. via `../` traversal or symlink).
#[allow(dead_code)]
fn validate_workspace_boundary(resolved: &Path, workspace_root: &Path) -> io::Result<()> {
    validate_workspace_boundary_impl(resolved, workspace_root, &[])
}

/// Like [`validate_workspace_boundary`] but also accepts a list of
/// additional path prefixes that are considered safe.  When the resolved
/// path starts with any of the `allowed` prefixes the check passes
/// immediately, even if the path is outside the workspace root.
///
/// This is used to honour user-configured `permissions.allow` rules that
/// explicitly grant access to directories outside the workspace.
#[allow(dead_code)]
fn validate_workspace_boundary_with_allowed(
    resolved: &Path,
    workspace_root: &Path,
    allowed: &[String],
) -> io::Result<()> {
    validate_workspace_boundary_impl(resolved, workspace_root, allowed)
}

fn validate_workspace_boundary_impl(
    resolved: &Path,
    workspace_root: &Path,
    allowed: &[String],
) -> io::Result<()> {
    // Normalize both paths to handle Windows \\?\ prefix inconsistencies
    let normalized_resolved = normalize_for_comparison(resolved);
    let normalized_root = normalize_for_comparison(workspace_root);

    if normalized_resolved.starts_with(&normalized_root) {
        return Ok(());
    }

    // Check whether the path matches any explicitly allowed external prefix.
    // Normalize path separators for cross-platform comparison.
    let resolved_str = normalize_separators(&normalized_resolved.to_string_lossy());
    for prefix in allowed {
        let normalized_prefix = normalize_for_comparison(&PathBuf::from(prefix));
        let prefix_str = normalize_separators(&normalized_prefix.to_string_lossy());
        // Ensure prefix ends with "/" for consistent matching - handles the case
        // where the allowed path is the directory itself (e.g., "path" matches "path/file")
        // Trim any existing trailing slash first to avoid "path//"
        let prefix_trimmed = prefix_str.trim_end_matches('/');
        if prefix_trimmed.is_empty() {
            // Edge case: prefix was just "/" - skip to avoid matching everything
            continue;
        }
        let prefix_with_slash = format!("{}/", prefix_trimmed);
        if resolved_str.starts_with(&prefix_with_slash) || resolved_str == prefix_trimmed {
            return Ok(());
        }
    }

    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "path {} escapes workspace boundary {}",
            resolved.display(),
            workspace_root.display()
        ),
    ))
}

/// Normalize path separators to forward slashes for cross-platform comparison.
fn normalize_separators(path: &str) -> String {
    path.replace('\\', "/")
}

/// Normalize a path for comparison by removing Windows \\?\ prefix if present.
#[cfg(target_os = "windows")]
fn normalize_for_comparison(path: &Path) -> PathBuf {
    let path_str = path.to_string_lossy();
    if path_str.starts_with("\\\\?\\") {
        PathBuf::from(&path_str[4..])
    } else {
        path.to_path_buf()
    }
}

#[cfg(not(target_os = "windows"))]
fn normalize_for_comparison(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// Text payload returned by file-reading operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TextFilePayload {
    #[serde(rename = "filePath")]
    pub file_path: String,
    pub content: String,
    #[serde(rename = "numLines")]
    pub num_lines: usize,
    #[serde(rename = "startLine")]
    pub start_line: usize,
    #[serde(rename = "totalLines")]
    pub total_lines: usize,
}

/// Output envelope for the `read_file` tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadFileOutput {
    #[serde(rename = "type")]
    pub kind: String,
    pub file: TextFilePayload,
}

/// Structured patch hunk emitted by write and edit operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StructuredPatchHunk {
    #[serde(rename = "oldStart")]
    pub old_start: usize,
    #[serde(rename = "oldLines")]
    pub old_lines: usize,
    #[serde(rename = "newStart")]
    pub new_start: usize,
    #[serde(rename = "newLines")]
    pub new_lines: usize,
    pub lines: Vec<String>,
}

/// Output envelope for full-file write operations.
///
/// **2026-07-17 DeepSeek 节制回执**（对齐 Reasonix `writefile.go` 不塞原文件的设计）：
/// `original_file` / `structured_patch` / `git_diff` 三个重型字段 `Option` + `skip_serializing_if`，
/// DeepSeek 后端走 `None` 路径让它们不出现在 JSON 回执里（只回 `"wrote <path>"`摘要+行号），
/// GLM 后端走 `Some` 路径保留原回执（GLM 容忍大回执且无字节级缓存击穿风险）。
/// 判定依据 `ANTHROPIC_MODEL` env 前缀：`deepseek` 开头走节制路径，`glm` 开头保持原状。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WriteFileOutput {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "filePath")]
    pub file_path: String,
    pub content: String,
    #[serde(rename = "structuredPatch", default, skip_serializing_if = "Option::is_none")]
    pub structured_patch: Option<Vec<StructuredPatchHunk>>,
    #[serde(rename = "originalFile", default, skip_serializing_if = "Option::is_none")]
    pub original_file: Option<String>,
    #[serde(rename = "gitDiff", default, skip_serializing_if = "Option::is_none")]
    pub git_diff: Option<serde_json::Value>,
}

/// 抽 `ANTHROPIC_MODEL` env 前缀判定回执节制策略。
/// `deepseek` 开头 → true（节制，对齐 Reasonix）；`glm` 开头或未设 → false（保持原回执）。
/// 在 runtime crate 内独立判断，不依赖 api crate（避免循环依赖）。
fn should_use_compact_receipt() -> bool {
    std::env::var("ANTHROPIC_MODEL")
        .ok()
        .map(|v| v.trim().to_lowercase().starts_with("deepseek"))
        .unwrap_or(false)
}

/// Output envelope for targeted string-replacement edits.
///
/// **2026-07-17 DeepSeek 节制回执**（对齐 Reasonix `editfile.go` 不塞原文件的设计）：
/// `original_file` / `structured_patch` / `git_diff` 三个重型字段改 `Option` + `skip_serializing_if`，
/// DeepSeek 后端走 `None` 路径让它们不出现在 JSON 回执里（只回 `"edited <path>"`摘要+行号区间），
/// GLM 后端走 `Some` 路径保留原回执。判定依据 `ANTHROPIC_MODEL` env 前缀。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EditFileOutput {
    #[serde(rename = "filePath")]
    pub file_path: String,
    #[serde(rename = "oldString")]
    pub old_string: String,
    #[serde(rename = "newString")]
    pub new_string: String,
    #[serde(rename = "originalFile", default, skip_serializing_if = "Option::is_none")]
    pub original_file: Option<String>,
    #[serde(rename = "structuredPatch", default, skip_serializing_if = "Option::is_none")]
    pub structured_patch: Option<Vec<StructuredPatchHunk>>,
    #[serde(rename = "userModified")]
    pub user_modified: bool,
    #[serde(rename = "replaceAll")]
    pub replace_all: bool,
    #[serde(rename = "gitDiff", default, skip_serializing_if = "Option::is_none")]
    pub git_diff: Option<serde_json::Value>,
}

/// Result of a glob-based filename search.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GlobSearchOutput {
    #[serde(rename = "durationMs")]
    pub duration_ms: u128,
    #[serde(rename = "numFiles")]
    pub num_files: usize,
    pub filenames: Vec<String>,
    pub truncated: bool,
}

/// Parameters accepted by the grep-style search tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GrepSearchInput {
    pub pattern: String,
    pub path: Option<String>,
    pub glob: Option<String>,
    #[serde(rename = "output_mode")]
    pub output_mode: Option<String>,
    #[serde(rename = "-B")]
    pub before: Option<usize>,
    #[serde(rename = "-A")]
    pub after: Option<usize>,
    #[serde(rename = "-C")]
    pub context_short: Option<usize>,
    pub context: Option<usize>,
    #[serde(rename = "-n")]
    pub line_numbers: Option<bool>,
    #[serde(rename = "-i")]
    pub case_insensitive: Option<bool>,
    #[serde(rename = "type")]
    pub file_type: Option<String>,
    pub head_limit: Option<usize>,
    pub offset: Option<usize>,
    pub multiline: Option<bool>,
}

/// Result payload returned by the grep-style search tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GrepSearchOutput {
    pub mode: Option<String>,
    #[serde(rename = "numFiles")]
    pub num_files: usize,
    pub filenames: Vec<String>,
    pub content: Option<String>,
    #[serde(rename = "numLines")]
    pub num_lines: Option<usize>,
    #[serde(rename = "numMatches")]
    pub num_matches: Option<usize>,
    #[serde(rename = "appliedLimit")]
    pub applied_limit: Option<usize>,
    #[serde(rename = "appliedOffset")]
    pub applied_offset: Option<usize>,
    #[serde(rename = "outputPersisted")]
    pub output_persisted: Option<bool>,
    #[serde(rename = "persistedPath")]
    pub persisted_path: Option<String>,
}

/// Reads a text file and returns a line-windowed payload.
pub fn read_file(
    path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> io::Result<ReadFileOutput> {
    let absolute_path = normalize_path(path)?;

    // Check file size before reading
    let metadata = fs::metadata(&absolute_path)?;
    if metadata.len() > MAX_READ_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "file is too large ({} bytes, max {} bytes)",
                metadata.len(),
                MAX_READ_SIZE
            ),
        ));
    }

    // Detect binary files
    if is_binary_file(&absolute_path)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file appears to be binary",
        ));
    }

    let content = fs::read_to_string(&absolute_path)?;
    let lines: Vec<&str> = content.lines().collect();
    let start_index = offset.unwrap_or(0).min(lines.len());
    let end_index = limit.map_or(lines.len(), |limit| {
        start_index.saturating_add(limit).min(lines.len())
    });
    let selected = lines[start_index..end_index].join("\n");

    Ok(ReadFileOutput {
        kind: String::from("text"),
        file: TextFilePayload {
            file_path: absolute_path.to_string_lossy().into_owned(),
            content: selected,
            num_lines: end_index.saturating_sub(start_index),
            start_line: start_index.saturating_add(1),
            total_lines: lines.len(),
        },
    })
}

/// Replaces a file's contents and returns patch metadata.
pub fn write_file(path: &str, content: &str) -> io::Result<WriteFileOutput> {
    if content.len() > MAX_WRITE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "content is too large ({} bytes, max {} bytes)",
                content.len(),
                MAX_WRITE_SIZE
            ),
        ));
    }

    let absolute_path = normalize_path_allow_missing(path)?;
    let original_file = fs::read_to_string(&absolute_path).ok();
    if let Some(parent) = absolute_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&absolute_path, content)?;

    // For large files, include first 10 lines + modified parts + last 10 lines
    let is_large_file =
        original_file.as_ref().map_or(false, |f| f.len() > 100000) || content.len() > 100000;

    let structured_patch = make_patch(original_file.as_deref().unwrap_or(""), content);

    // 2026-07-17 DeepSeek 节制回执：DeepSeek 后端走 None 路径省掉 original_file + structured_patch
    // （对齐 Reasonix writefile.go 只回 "wrote <path>" + 行数摘要的设计）。
    // GLM 后端保持原回执（GLM 容忍大回执且无字节级缓存击穿风险）。
    let compact = should_use_compact_receipt();
    let original_file_output = if compact {
        None
    } else {
        original_file.as_ref().map(|f| {
            if is_large_file {
                format!("[File content omitted - {} bytes]", f.len())
            } else {
                f.clone()
            }
        })
    };
    let structured_patch_output = if compact { None } else { Some(structured_patch) };

    Ok(WriteFileOutput {
        kind: if original_file.is_some() {
            String::from("update")
        } else {
            String::from("create")
        },
        file_path: absolute_path.to_string_lossy().into_owned(),
        content: content.to_owned(),
        structured_patch: structured_patch_output,
        original_file: original_file_output,
        git_diff: None,
    })
}

/// Performs an in-file string replacement and returns patch metadata.
pub fn edit_file(
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> io::Result<EditFileOutput> {
    let absolute_path = normalize_path(path)?;
    let original_file = fs::read_to_string(&absolute_path)?;
    if old_string == new_string {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "old_string and new_string must differ",
        ));
    }

    // Normalize line endings: convert old_string to match file's line endings
    let (normalized_old, normalized_new) =
        if original_file.contains("\r\n") && !old_string.contains("\r\n") {
            // File uses \r\n, but old_string uses \n - convert old_string and new_string
            (
                old_string.replace("\n", "\r\n"),
                new_string.replace("\n", "\r\n"),
            )
        } else if !original_file.contains("\r\n") && old_string.contains("\r\n") {
            // File uses \n, but old_string uses \r\n - convert old_string and new_string
            (
                old_string.replace("\r\n", "\n"),
                new_string.replace("\r\n", "\n"),
            )
        } else {
            (old_string.to_string(), new_string.to_string())
        };

    if !original_file.contains(normalized_old.as_str()) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "old_string not found in file",
        ));
    }

    let updated = if replace_all {
        original_file.replace(normalized_old.as_str(), normalized_new.as_str())
    } else {
        original_file.replacen(normalized_old.as_str(), normalized_new.as_str(), 1)
    };
    fs::write(&absolute_path, &updated)?;

    // For large files, don't include the full original_file and patch in the output
    // This reduces the output size and avoids API limits
    // For large files, include first 10 lines + modified parts + last 10 lines
    let is_large_file = original_file.len() > 100000;

    let structured_patch = make_patch(&original_file, &updated);

    // 2026-07-17 DeepSeek 节制回执：DeepSeek 后端走 None 路径省掉 original_file + structured_patch
    // （对齐 Reasonix editfile.go 只回 "edited <path>" + receipt 的设计，不塞原文件让模型 verify diff）。
    // GLM 后端保持原回执（GLM 容忍大回执且无字节级缓存击穿风险）。
    let compact = should_use_compact_receipt();
    let original_file_output = if compact {
        None
    } else if is_large_file {
        Some(format!("[File content omitted - {} bytes]", original_file.len()))
    } else {
        Some(original_file.clone())
    };
    let structured_patch_output = if compact { None } else { Some(structured_patch) };

    Ok(EditFileOutput {
        file_path: absolute_path.to_string_lossy().into_owned(),
        old_string: old_string.to_owned(),
        new_string: new_string.to_owned(),
        original_file: original_file_output,
        structured_patch: structured_patch_output,
        user_modified: false,
        replace_all,
        git_diff: None,
    })
}

/// Expands a glob pattern and returns matching filenames.
pub fn glob_search(pattern: &str, path: Option<&str>) -> io::Result<GlobSearchOutput> {
    glob_search_impl(pattern, path, None)
}

fn glob_search_impl(
    pattern: &str,
    path: Option<&str>,
    workspace_root: Option<&Path>,
) -> io::Result<GlobSearchOutput> {
    glob_search_impl_with_allowed(pattern, path, workspace_root, &[])
}

fn glob_search_impl_with_allowed(
    pattern: &str,
    path: Option<&str>,
    workspace_root: Option<&Path>,
    allowed: &[String],
) -> io::Result<GlobSearchOutput> {
    let started = Instant::now();
    let base_dir = path
        .map(normalize_path)
        .transpose()?
        .unwrap_or(std::env::current_dir()?);
    let canonical_root = workspace_root.map(canonicalize_workspace_root);
    if let Some(root) = canonical_root.as_deref() {
        validate_workspace_boundary_with_allowed(&base_dir, root, allowed)?;
    }

    // Build the search pattern, handling Windows extended-length path prefix
    let search_pattern = if Path::new(pattern).is_absolute() {
        // Strip Windows \\?\ prefix if present before using with glob
        if pattern.starts_with("\\\\?\\") {
            pattern[4..].to_owned()
        } else {
            pattern.to_owned()
        }
    } else {
        base_dir.join(pattern).to_string_lossy().into_owned()
    };

    // Also strip \\?\ prefix from search_pattern if it came from base_dir
    let search_pattern = if search_pattern.starts_with("\\\\?\\") {
        search_pattern[4..].to_owned()
    } else {
        search_pattern
    };

    // The `glob` crate does not support brace expansion ({a,b,c}).
    // Expand braces into multiple patterns so patterns like
    // `Assets/**/*.{cs,uxml,uss}` work correctly.
    let expanded = expand_braces(&search_pattern);

    let mut seen = HashSet::new();
    let mut matches = Vec::new();
    for pat in &expanded {
        let compiled = Pattern::new(pat)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
        let walk_root = derive_glob_walk_root(pat);

        if let Some(root) = canonical_root.as_deref() {
            let canonical_walk_root = walk_root
                .canonicalize()
                .unwrap_or_else(|_| walk_root.clone());
            validate_workspace_boundary_with_allowed(&canonical_walk_root, root, allowed)?;
        }
        let entries = WalkDir::new(&walk_root)
            .into_iter()
            .filter_entry(|entry| !should_skip_glob_dir(entry));
        for entry in entries.flatten() {
            let candidate = entry.path();
            if entry.file_type().is_file()
                && compiled.matches_path(candidate)
                && seen.insert(candidate.to_path_buf())
            {
                if let Some(root) = canonical_root.as_deref() {
                    let canonical_candidate = candidate.canonicalize()?;
                    validate_workspace_boundary_with_allowed(&canonical_candidate, root, allowed)?;
                }
                matches.push(candidate.to_path_buf());
            }
        }
    }

    matches.sort_by_key(|path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .map(Reverse)
    });

    let truncated = matches.len() > 100;
    let filenames = matches
        .into_iter()
        .take(100)
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    Ok(GlobSearchOutput {
        duration_ms: started.elapsed().as_millis(),
        num_files: filenames.len(),
        filenames,
        truncated,
    })
}

/// Runs a regex search over workspace files with optional context lines.
pub fn grep_search(input: &GrepSearchInput) -> io::Result<GrepSearchOutput> {
    grep_search_impl(input, None)
}

fn grep_search_impl(
    input: &GrepSearchInput,
    workspace_root: Option<&Path>,
) -> io::Result<GrepSearchOutput> {
    grep_search_impl_with_allowed(input, workspace_root, &[])
}

fn grep_search_impl_with_allowed(
    input: &GrepSearchInput,
    workspace_root: Option<&Path>,
    allowed: &[String],
) -> io::Result<GrepSearchOutput> {
    let base_path = input
        .path
        .as_deref()
        .map(normalize_path)
        .transpose()?
        .unwrap_or(std::env::current_dir()?);
    let canonical_root = workspace_root.map(canonicalize_workspace_root);
    if let Some(root) = canonical_root.as_deref() {
        validate_workspace_boundary_with_allowed(&base_path, root, allowed)?;
    }

    let regex = RegexBuilder::new(&input.pattern)
        .case_insensitive(input.case_insensitive.unwrap_or(false))
        .dot_matches_new_line(input.multiline.unwrap_or(false))
        .build()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;

    let glob_filter = input
        .glob
        .as_deref()
        .map(Pattern::new)
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    let file_type = input.file_type.as_deref();
    let output_mode = input
        .output_mode
        .clone()
        .unwrap_or_else(|| String::from("files_with_matches"));
    let context = input.context.or(input.context_short).unwrap_or(0);

    let mut filenames = Vec::new();
    let mut content_lines = Vec::new();
    let mut total_matches = 0usize;

    for file_path in collect_search_files(&base_path)? {
        if let Some(root) = canonical_root.as_deref() {
            let canonical_file = file_path.canonicalize()?;
            validate_workspace_boundary_with_allowed(&canonical_file, root, allowed)?;
        }
        if !matches_optional_filters(&file_path, glob_filter.as_ref(), file_type) {
            continue;
        }

        let Ok(file_contents) = fs::read_to_string(&file_path) else {
            continue;
        };

        if output_mode == "count" {
            let count = regex.find_iter(&file_contents).count();
            if count > 0 {
                filenames.push(file_path.to_string_lossy().into_owned());
                total_matches += count;
            }
            continue;
        }

        let lines: Vec<&str> = file_contents.lines().collect();
        let mut matched_lines = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            if regex.is_match(line) {
                total_matches += 1;
                matched_lines.push(index);
            }
        }

        if matched_lines.is_empty() {
            continue;
        }

        filenames.push(file_path.to_string_lossy().into_owned());
        if output_mode == "content" {
            for index in matched_lines {
                let start = index.saturating_sub(input.before.unwrap_or(context));
                let end = (index + input.after.unwrap_or(context) + 1).min(lines.len());
                for (current, line) in lines.iter().enumerate().take(end).skip(start) {
                    let prefix = if input.line_numbers.unwrap_or(true) {
                        format!("{}:{}:", file_path.to_string_lossy(), current + 1)
                    } else {
                        format!("{}:", file_path.to_string_lossy())
                    };
                    content_lines.push(format!("{prefix}{line}"));
                }
            }
        }
    }

    let (filenames, applied_limit, applied_offset) =
        apply_limit(filenames, input.head_limit, input.offset);
    if output_mode == "content" {
        return Ok(build_grep_content_output(
            output_mode,
            filenames,
            content_lines,
            input.head_limit,
            input.offset,
            total_matches,
        ));
    }

    Ok(GrepSearchOutput {
        mode: Some(output_mode.clone()),
        num_files: filenames.len(),
        filenames,
        content: None,
        num_lines: None,
        // Report total matches in every mode so the CLI summary line
        // ("N matches across M files") is correct even for the default
        // `files_with_matches` mode, which previously left this as None
        // and rendered as "0 matches".
        num_matches: Some(total_matches),
        applied_limit,
        applied_offset,
        output_persisted: None,
        persisted_path: None,
    })
}

/// Persist large tool output to disk and return the file path.
///
/// The file is written under `<workspace_root>/.claw/persisted/grep_results/`
/// with a unique name derived from the current timestamp and a hash of the
/// content.  If the directory cannot be created or the file cannot be written,
/// the function returns `None` (the caller should fall back to inline output).
fn persist_large_output(content: &str) -> Option<String> {
    let workspace_root = std::env::current_dir().ok()?;
    let persisted_dir = workspace_root
        .join(".claw")
        .join("persisted")
        .join("grep_results");
    fs::create_dir_all(&persisted_dir).ok()?;

    // Generate a unique filename using timestamp and a short hash of the content.
    // Use only alphanumeric characters to avoid issues with special characters
    // (e.g., "-" in the hash might get mangled by hooks or other processing).
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis();
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    let content_hash = hasher.finish();
    // Use hex encoding but remove non-alphanumeric characters
    let hash_hex = format!("{:016x}", content_hash);
    let hash_clean: String = hash_hex.chars().filter(|c| c.is_alphanumeric()).collect();
    let filename = format!(
        "grep_{timestamp}_{}.txt",
        &hash_clean[..hash_clean.len().min(16)]
    );
    let file_path = persisted_dir.join(&filename);

    fs::write(&file_path, content).ok()?;

    // Return a relative-style path that the LLM can use with read_file.
    Some(format!(".claw/persisted/grep_results/{filename}"))
}

fn build_grep_content_output(
    output_mode: String,
    filenames: Vec<String>,
    content_lines: Vec<String>,
    head_limit: Option<usize>,
    offset: Option<usize>,
    total_matches: usize,
) -> GrepSearchOutput {
    let (lines, limit, offset) = apply_limit(content_lines, head_limit, offset);
    let content = lines.join("\n");

    // If the content exceeds the size threshold, persist it to disk
    // and replace the inline content with a file reference so the LLM
    // can read it on demand instead of blowing up the context window.
    let char_count = content.chars().count();
    if char_count > DEFAULT_MAX_RESULT_SIZE_CHARS {
        if let Some(persisted_path) = persist_large_output(&content) {
            return GrepSearchOutput {
                mode: Some(output_mode),
                num_files: filenames.len(),
                filenames,
                num_lines: Some(lines.len()),
                content: Some(format!(
                    "[Large output persisted to file: {persisted_path}]"
                )),
                num_matches: Some(total_matches),
                applied_limit: limit,
                applied_offset: offset,
                output_persisted: Some(true),
                persisted_path: Some(persisted_path),
            };
        }
        // If persistence failed, fall through to return the full content inline.
    }

    GrepSearchOutput {
        mode: Some(output_mode),
        num_files: filenames.len(),
        filenames,
        num_lines: Some(lines.len()),
        content: Some(content),
        num_matches: Some(total_matches),
        applied_limit: limit,
        applied_offset: offset,
        output_persisted: None,
        persisted_path: None,
    }
}

fn canonicalize_workspace_root(workspace_root: &Path) -> PathBuf {
    workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf())
}

fn should_skip_glob_dir(entry: &DirEntry) -> bool {
    entry.file_type().is_dir()
        && entry
            .file_name()
            .to_str()
            .is_some_and(|name| GLOB_SEARCH_IGNORED_DIRS.contains(&name))
}

fn derive_glob_walk_root(pattern: &str) -> PathBuf {
    // Strip Windows extended-length path prefix if present
    let pattern_stripped = if pattern.starts_with("\\\\?\\") {
        &pattern[4..]
    } else {
        pattern
    };

    let path = Path::new(pattern_stripped);
    let mut prefix = PathBuf::new();
    let mut saw_component = false;

    for component in path.components() {
        let text = component.as_os_str().to_string_lossy();
        if component_contains_glob(&text) {
            break;
        }
        prefix.push(component.as_os_str());
        saw_component = true;
    }

    if saw_component {
        prefix
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }
}

fn component_contains_glob(component: &str) -> bool {
    component.contains('*') || component.contains('?') || component.contains('[')
}

fn collect_search_files(base_path: &Path) -> io::Result<Vec<PathBuf>> {
    if base_path.is_file() {
        return Ok(vec![base_path.to_path_buf()]);
    }

    let mut files = Vec::new();
    for entry in WalkDir::new(base_path)
        .into_iter()
        .filter_entry(|entry| !should_skip_glob_dir(entry))
    {
        let entry = entry.map_err(|error| io::Error::other(error.to_string()))?;
        if entry.file_type().is_file() {
            files.push(entry.path().to_path_buf());
        }
    }
    Ok(files)
}

fn matches_optional_filters(
    path: &Path,
    glob_filter: Option<&Pattern>,
    file_type: Option<&str>,
) -> bool {
    if let Some(glob_filter) = glob_filter {
        let path_string = path.to_string_lossy();
        if !glob_filter.matches(&path_string) && !glob_filter.matches_path(path) {
            return false;
        }
    }

    if let Some(file_type) = file_type {
        let extension = path.extension().and_then(|extension| extension.to_str());
        if extension != Some(file_type) {
            return false;
        }
    }

    true
}

fn apply_limit<T>(
    items: Vec<T>,
    limit: Option<usize>,
    offset: Option<usize>,
) -> (Vec<T>, Option<usize>, Option<usize>) {
    let offset_value = offset.unwrap_or(0);
    let mut items = items.into_iter().skip(offset_value).collect::<Vec<_>>();
    let explicit_limit = limit.unwrap_or(250);
    if explicit_limit == 0 {
        return (items, None, (offset_value > 0).then_some(offset_value));
    }

    let truncated = items.len() > explicit_limit;
    items.truncate(explicit_limit);
    (
        items,
        truncated.then_some(explicit_limit),
        (offset_value > 0).then_some(offset_value),
    )
}

fn make_patch(original: &str, updated: &str) -> Vec<StructuredPatchHunk> {
    let original_lines: Vec<&str> = original.lines().collect();
    let updated_lines: Vec<&str> = updated.lines().collect();

    // For small files, include all lines
    if original_lines.len() <= 100 {
        let mut lines = Vec::new();
        for line in &original_lines {
            lines.push(format!("-{line}"));
        }
        for line in &updated_lines {
            lines.push(format!("+{line}"));
        }
        return vec![StructuredPatchHunk {
            old_start: 1,
            old_lines: original_lines.len(),
            new_start: 1,
            new_lines: updated_lines.len(),
            lines,
        }];
    }

    // For large files, include first 10 lines + changed sections + last 10 lines
    let header_lines = 10;
    let footer_lines = 10;
    let mut hunks = Vec::new();

    // First hunk: first 10 lines
    let first_chunk_orig: Vec<String> = original_lines
        .iter()
        .take(header_lines)
        .map(|l| format!("-{l}"))
        .collect();
    let first_chunk_upd: Vec<String> = updated_lines
        .iter()
        .take(header_lines)
        .map(|l| format!("+{l}"))
        .collect();

    let mut first_lines = Vec::new();
    first_lines.extend(first_chunk_orig);
    first_lines.extend(first_chunk_upd);

    hunks.push(StructuredPatchHunk {
        old_start: 1,
        old_lines: header_lines.min(original_lines.len()),
        new_start: 1,
        new_lines: header_lines.min(updated_lines.len()),
        lines: first_lines,
    });

    // Middle hunks: changed sections
    let mut i = 0;
    let mut j = 0;

    while i < original_lines.len() || j < updated_lines.len() {
        // Skip matching lines
        while i < original_lines.len()
            && j < updated_lines.len()
            && original_lines[i] == updated_lines[j]
        {
            i += 1;
            j += 1;
        }

        if i >= original_lines.len() && j >= updated_lines.len() {
            break;
        }

        // Found a difference - collect the changed region
        let change_start_orig = i;
        let change_start_upd = j;

        // Find end of changed region
        while i < original_lines.len()
            && j < updated_lines.len()
            && original_lines[i] != updated_lines[j]
        {
            i += 1;
            j += 1;
        }

        // Also handle additions or deletions
        while i < original_lines.len()
            && (j >= updated_lines.len() || original_lines[i] != updated_lines[j])
        {
            i += 1;
        }
        while j < updated_lines.len()
            && (i >= original_lines.len() || original_lines[i] != updated_lines[j])
        {
            j += 1;
        }

        let change_end_orig = i;
        let change_end_upd = j;

        // Build hunk lines for changed section
        let mut hunk_lines = Vec::new();

        // Removed lines
        for idx in change_start_orig..change_end_orig {
            hunk_lines.push(format!("-{}", original_lines[idx]));
        }

        // Added lines
        for idx in change_start_upd..change_end_upd {
            hunk_lines.push(format!("+{}", updated_lines[idx]));
        }

        hunks.push(StructuredPatchHunk {
            old_start: change_start_orig + 1,
            old_lines: change_end_orig - change_start_orig,
            new_start: change_start_upd + 1,
            new_lines: change_end_upd - change_start_upd,
            lines: hunk_lines,
        });
    }

    // Last hunk: last 10 lines
    let last_chunk_orig: Vec<String> = original_lines
        .iter()
        .rev()
        .take(footer_lines)
        .rev()
        .map(|l| format!("-{l}"))
        .collect();
    let last_chunk_upd: Vec<String> = updated_lines
        .iter()
        .rev()
        .take(footer_lines)
        .rev()
        .map(|l| format!("+{l}"))
        .collect();

    let mut last_lines = Vec::new();
    last_lines.extend(last_chunk_orig);
    last_lines.extend(last_chunk_upd);

    hunks.push(StructuredPatchHunk {
        old_start: original_lines.len().saturating_sub(footer_lines) + 1,
        old_lines: footer_lines.min(original_lines.len()),
        new_start: updated_lines.len().saturating_sub(footer_lines) + 1,
        new_lines: footer_lines.min(updated_lines.len()),
        lines: last_lines,
    });

    hunks
}

fn normalize_path(path: &str) -> io::Result<PathBuf> {
    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        std::env::current_dir()?.join(path)
    };
    // canonicalize resolves symlinks and normalises the path, but it requires
    // the path to exist *and* to be resolvable by the OS.  On Windows, paths
    // with forward slashes and CJK characters can fail canonicalize even when
    // the directory exists (os error 2).  Fall back to the un-canonicalized
    // candidate so the operation can still proceed — the path is valid, just
    // not in its canonical form.
    candidate.canonicalize().or_else(|_| {
        // If the parent exists, at least normalise that part.
        if let Some(parent) = candidate.parent() {
            if let Ok(canonical_parent) = parent.canonicalize() {
                if let Some(name) = candidate.file_name() {
                    return Ok(canonical_parent.join(name));
                }
            }
        }
        Ok(candidate)
    })
}

fn normalize_path_allow_missing(path: &str) -> io::Result<PathBuf> {
    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        std::env::current_dir()?.join(path)
    };

    if let Ok(canonical) = candidate.canonicalize() {
        return Ok(canonical);
    }

    if let Some(parent) = candidate.parent() {
        let canonical_parent = parent
            .canonicalize()
            .unwrap_or_else(|_| parent.to_path_buf());
        if let Some(name) = candidate.file_name() {
            return Ok(canonical_parent.join(name));
        }
    }

    Ok(candidate)
}

/// Read a file with workspace boundary enforcement.
#[allow(dead_code)]
pub fn read_file_in_workspace(
    path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    workspace_root: &Path,
) -> io::Result<ReadFileOutput> {
    read_file_in_workspace_with_allowed(path, offset, limit, workspace_root, &[])
}

/// Read a file with workspace boundary enforcement, honouring allowed
/// external path prefixes from the permission system.
pub fn read_file_in_workspace_with_allowed(
    path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    workspace_root: &Path,
    allowed: &[String],
) -> io::Result<ReadFileOutput> {
    let absolute_path = normalize_path(path)?;
    let canonical_root = canonicalize_workspace_root(workspace_root);
    validate_workspace_boundary_with_allowed(&absolute_path, &canonical_root, allowed)?;
    read_file(path, offset, limit)
}

/// Write a file with workspace boundary enforcement.
#[allow(dead_code)]
pub fn write_file_in_workspace(
    path: &str,
    content: &str,
    workspace_root: &Path,
) -> io::Result<WriteFileOutput> {
    write_file_in_workspace_with_allowed(path, content, workspace_root, &[])
}

/// Write a file with workspace boundary enforcement, honouring allowed
/// external path prefixes from the permission system.
pub fn write_file_in_workspace_with_allowed(
    path: &str,
    content: &str,
    workspace_root: &Path,
    allowed: &[String],
) -> io::Result<WriteFileOutput> {
    let absolute_path = normalize_path_allow_missing(path)?;
    let canonical_root = canonicalize_workspace_root(workspace_root);
    validate_workspace_boundary_with_allowed(&absolute_path, &canonical_root, allowed)?;
    write_file(path, content)
}

/// Edit a file with workspace boundary enforcement.
#[allow(dead_code)]
pub fn edit_file_in_workspace(
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    workspace_root: &Path,
) -> io::Result<EditFileOutput> {
    edit_file_in_workspace_with_allowed(
        path,
        old_string,
        new_string,
        replace_all,
        workspace_root,
        &[],
    )
}

/// Edit a file with workspace boundary enforcement, honouring allowed
/// external path prefixes from the permission system.
pub fn edit_file_in_workspace_with_allowed(
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    workspace_root: &Path,
    allowed: &[String],
) -> io::Result<EditFileOutput> {
    let absolute_path = normalize_path(path)?;
    let canonical_root = canonicalize_workspace_root(workspace_root);
    validate_workspace_boundary_with_allowed(&absolute_path, &canonical_root, allowed)?;
    edit_file(path, old_string, new_string, replace_all)
}

/// Expand a glob pattern with workspace boundary enforcement.
#[allow(dead_code)]
pub fn glob_search_in_workspace(
    pattern: &str,
    path: Option<&str>,
    workspace_root: &Path,
) -> io::Result<GlobSearchOutput> {
    glob_search_in_workspace_with_allowed(pattern, path, workspace_root, &[])
}

/// Expand a glob pattern with workspace boundary enforcement, honouring
/// allowed external path prefixes from the permission system.
pub fn glob_search_in_workspace_with_allowed(
    pattern: &str,
    path: Option<&str>,
    workspace_root: &Path,
    allowed: &[String],
) -> io::Result<GlobSearchOutput> {
    glob_search_impl_with_allowed(pattern, path, Some(workspace_root), allowed)
}

/// Search file contents with workspace boundary enforcement.
#[allow(dead_code)]
pub fn grep_search_in_workspace(
    input: &GrepSearchInput,
    workspace_root: &Path,
) -> io::Result<GrepSearchOutput> {
    grep_search_in_workspace_with_allowed(input, workspace_root, &[])
}

/// Search file contents with workspace boundary enforcement, honouring
/// allowed external path prefixes from the permission system.
pub fn grep_search_in_workspace_with_allowed(
    input: &GrepSearchInput,
    workspace_root: &Path,
    allowed: &[String],
) -> io::Result<GrepSearchOutput> {
    grep_search_impl_with_allowed(input, Some(workspace_root), allowed)
}

/// Check whether a path is a symlink that resolves outside the workspace.
#[allow(dead_code)]
pub fn is_symlink_escape(path: &Path, workspace_root: &Path) -> io::Result<bool> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_symlink() {
        return Ok(false);
    }
    let resolved = path.canonicalize()?;
    let canonical_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    Ok(!resolved.starts_with(&canonical_root))
}

/// Expand shell-style brace groups in a glob pattern.
///
/// Handles one level of braces: `foo.{a,b,c}` → `["foo.a", "foo.b", "foo.c"]`.
/// Nested braces are not expanded (uncommon in practice).
/// Patterns without braces pass through unchanged.
fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else {
        return vec![pattern.to_owned()];
    };
    let Some(close) = pattern[open..].find('}').map(|i| open + i) else {
        // Unmatched brace — treat as literal.
        return vec![pattern.to_owned()];
    };
    let prefix = &pattern[..open];
    let suffix = &pattern[close + 1..];
    let alternatives = &pattern[open + 1..close];
    alternatives
        .split(',')
        .flat_map(|alt| expand_braces(&format!("{prefix}{alt}{suffix}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        component_contains_glob, derive_glob_walk_root, edit_file, expand_braces, glob_search,
        grep_search, is_symlink_escape, read_file, read_file_in_workspace, write_file,
        write_file_in_workspace, GrepSearchInput, MAX_WRITE_SIZE,
    };

    fn temp_path(name: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should move forward")
            .as_nanos();
        std::env::temp_dir().join(format!("clawd-native-{name}-{unique}"))
    }

    #[test]
    fn reads_and_writes_files() {
        let path = temp_path("read-write.txt");
        let write_output = write_file(path.to_string_lossy().as_ref(), "one\ntwo\nthree")
            .expect("write should succeed");
        assert_eq!(write_output.kind, "create");

        let read_output = read_file(path.to_string_lossy().as_ref(), Some(1), Some(1))
            .expect("read should succeed");
        assert_eq!(read_output.file.content, "two");
    }

    #[test]
    fn edits_file_contents() {
        let path = temp_path("edit.txt");
        write_file(path.to_string_lossy().as_ref(), "alpha beta alpha")
            .expect("initial write should succeed");
        let output = edit_file(path.to_string_lossy().as_ref(), "alpha", "omega", true)
            .expect("edit should succeed");
        assert!(output.replace_all);
    }

    #[test]
    fn rejects_binary_files() {
        let path = temp_path("binary-test.bin");
        std::fs::write(&path, b"\x00\x01\x02\x03binary content").expect("write should succeed");
        let result = read_file(path.to_string_lossy().as_ref(), None, None);
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("binary"));
    }

    #[test]
    fn rejects_oversized_writes() {
        let path = temp_path("oversize-write.txt");
        let huge = "x".repeat(MAX_WRITE_SIZE + 1);
        let result = write_file(path.to_string_lossy().as_ref(), &huge);
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("too large"));
    }

    #[test]
    fn enforces_workspace_boundary() {
        let workspace = temp_path("workspace-boundary");
        std::fs::create_dir_all(&workspace).expect("workspace dir should be created");
        let inside = workspace.join("inside.txt");
        write_file(inside.to_string_lossy().as_ref(), "safe content")
            .expect("write inside workspace should succeed");

        // Reading inside workspace should succeed
        let result =
            read_file_in_workspace(inside.to_string_lossy().as_ref(), None, None, &workspace);
        assert!(result.is_ok());

        // Reading outside workspace should fail
        let outside = temp_path("outside-boundary.txt");
        write_file(outside.to_string_lossy().as_ref(), "unsafe content")
            .expect("write outside should succeed");
        let result =
            read_file_in_workspace(outside.to_string_lossy().as_ref(), None, None, &workspace);
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("escapes workspace"));
    }

    #[test]
    fn detects_symlink_escape() {
        let workspace = temp_path("symlink-workspace");
        std::fs::create_dir_all(&workspace).expect("workspace dir should be created");
        let outside = temp_path("symlink-target.txt");
        std::fs::write(&outside, "target content").expect("target should write");

        let link_path = workspace.join("escape-link.txt");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, &link_path).expect("symlink should create");
            assert!(is_symlink_escape(&link_path, &workspace).expect("check should succeed"));
        }

        // Non-symlink file should not be an escape
        let normal = workspace.join("normal.txt");
        std::fs::write(&normal, "normal content").expect("normal file should write");
        assert!(!is_symlink_escape(&normal, &workspace).expect("check should succeed"));
    }

    #[test]
    #[cfg(unix)]
    fn workspace_read_rejects_symlink_escape_regression_3007_class() {
        let workspace = temp_path("workspace-read-symlink-escape");
        let outside = temp_path("workspace-read-symlink-target");
        std::fs::create_dir_all(&workspace).expect("workspace dir should be created");
        std::fs::create_dir_all(&outside).expect("outside dir should be created");
        let outside_file = outside.join("secret.txt");
        std::fs::write(&outside_file, "outside secret").expect("outside file should write");

        let link_path = workspace.join("linked-secret.txt");
        std::os::unix::fs::symlink(&outside_file, &link_path).expect("symlink should create");

        let result =
            read_file_in_workspace(link_path.to_string_lossy().as_ref(), None, None, &workspace);

        assert!(result.is_err(), "symlink escape must be rejected");
        let error = result.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            error.to_string().contains("escapes workspace"),
            "error should explain workspace escape: {error}"
        );

        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    #[cfg(unix)]
    fn workspace_write_rejects_parent_symlink_escape_regression_3007_class() {
        let workspace = temp_path("workspace-write-symlink-escape");
        let outside = temp_path("workspace-write-symlink-target");
        std::fs::create_dir_all(&workspace).expect("workspace dir should be created");
        std::fs::create_dir_all(&outside).expect("outside dir should be created");

        let link_dir = workspace.join("linked-outside");
        std::os::unix::fs::symlink(&outside, &link_dir).expect("symlink dir should create");
        let escaped_child = link_dir.join("created.txt");

        let result = write_file_in_workspace(
            escaped_child.to_string_lossy().as_ref(),
            "must not escape",
            &workspace,
        );

        assert!(result.is_err(), "parent symlink escape must be rejected");
        let error = result.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            error.to_string().contains("escapes workspace"),
            "error should explain workspace escape: {error}"
        );
        assert!(
            !outside.join("created.txt").exists(),
            "write should not create through an escaping symlink"
        );

        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn globs_and_greps_directory() {
        let dir = temp_path("search-dir");
        std::fs::create_dir_all(&dir).expect("directory should be created");
        let file = dir.join("demo.rs");
        write_file(
            file.to_string_lossy().as_ref(),
            "fn main() {\n println!(\"hello\");\n}\n",
        )
        .expect("file write should succeed");

        let globbed = glob_search("**/*.rs", Some(dir.to_string_lossy().as_ref()))
            .expect("glob should succeed");
        assert_eq!(globbed.num_files, 1);

        let grep_output = grep_search(&GrepSearchInput {
            pattern: String::from("hello"),
            path: Some(dir.to_string_lossy().into_owned()),
            glob: Some(String::from("**/*.rs")),
            output_mode: Some(String::from("content")),
            before: None,
            after: None,
            context_short: None,
            context: None,
            line_numbers: Some(true),
            case_insensitive: Some(false),
            file_type: None,
            head_limit: Some(10),
            offset: Some(0),
            multiline: Some(false),
        })
        .expect("grep should succeed");
        assert!(grep_output.content.unwrap_or_default().contains("hello"));
    }

    #[test]
    fn expand_braces_no_braces() {
        assert_eq!(expand_braces("*.rs"), vec!["*.rs"]);
    }

    #[test]
    fn expand_braces_single_group() {
        let mut result = expand_braces("Assets/**/*.{cs,uxml,uss}");
        result.sort();
        assert_eq!(
            result,
            vec!["Assets/**/*.cs", "Assets/**/*.uss", "Assets/**/*.uxml",]
        );
    }

    #[test]
    fn expand_braces_nested() {
        let mut result = expand_braces("src/{a,b}.{rs,toml}");
        result.sort();
        assert_eq!(
            result,
            vec!["src/a.rs", "src/a.toml", "src/b.rs", "src/b.toml"]
        );
    }

    #[test]
    fn expand_braces_unmatched() {
        assert_eq!(expand_braces("foo.{bar"), vec!["foo.{bar"]);
    }

    #[test]
    fn glob_search_with_braces_finds_files() {
        let dir = temp_path("glob-braces");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.join("b.toml"), "[package]").unwrap();
        std::fs::write(dir.join("c.txt"), "hello").unwrap();

        let result =
            glob_search("*.{rs,toml}", Some(dir.to_str().unwrap())).expect("glob should succeed");
        assert_eq!(
            result.num_files, 2,
            "should match .rs and .toml but not .txt"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn glob_search_skips_common_heavy_directories() {
        let dir = temp_path("glob-ignored-dirs");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(dir.join(".build/checkouts/pkg")).unwrap();
        std::fs::create_dir_all(dir.join("target/debug/deps")).unwrap();

        std::fs::write(dir.join("src/AGENTS.md"), "src").unwrap();
        std::fs::write(dir.join("docs/AGENTS.md"), "docs").unwrap();
        std::fs::write(dir.join("node_modules/pkg/AGENTS.md"), "node_modules").unwrap();
        std::fs::write(dir.join(".build/checkouts/pkg/AGENTS.md"), ".build").unwrap();
        std::fs::write(dir.join("target/debug/deps/AGENTS.md"), "target").unwrap();

        let result =
            glob_search("**/AGENTS.md", Some(dir.to_str().unwrap())).expect("glob should succeed");

        assert_eq!(result.num_files, 2, "ignored dirs should be pruned");
        assert!(result
            .filenames
            .iter()
            .any(|path| path.ends_with("src/AGENTS.md")));
        assert!(result
            .filenames
            .iter()
            .any(|path| path.ends_with("docs/AGENTS.md")));
        assert!(!result
            .filenames
            .iter()
            .any(|path| path.contains("node_modules")
                || path.contains(".build")
                || path.contains("/target/")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn derive_glob_walk_root_stops_at_first_glob_component() {
        let root = derive_glob_walk_root("/tmp/demo/**/AGENTS.md");
        assert_eq!(root, PathBuf::from("/tmp/demo"));
        assert!(component_contains_glob("**"));
        assert!(component_contains_glob("*.rs"));
        assert!(!component_contains_glob("src"));
    }
}

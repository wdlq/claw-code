use std::env;
use std::io;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::process::Command as TokioCommand;
use tokio::runtime::Builder;
use tokio::time::timeout;

use crate::lane_events::{LaneEvent, ShipMergeMethod, ShipProvenance};
use crate::sandbox::{
    build_linux_sandbox_command, resolve_sandbox_status_for_request, FilesystemIsolationMode,
    SandboxConfig, SandboxStatus,
};
use crate::ConfigLoader;

/// Input schema for the built-in bash execution tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BashCommandInput {
    pub command: String,
    pub timeout: Option<u64>,
    pub description: Option<String>,
    #[serde(rename = "run_in_background")]
    pub run_in_background: Option<bool>,
    #[serde(rename = "dangerouslyDisableSandbox")]
    pub dangerously_disable_sandbox: Option<bool>,
    #[serde(rename = "namespaceRestrictions")]
    pub namespace_restrictions: Option<bool>,
    #[serde(rename = "isolateNetwork")]
    pub isolate_network: Option<bool>,
    #[serde(rename = "filesystemMode")]
    pub filesystem_mode: Option<FilesystemIsolationMode>,
    #[serde(rename = "allowedMounts")]
    pub allowed_mounts: Option<Vec<String>>,
}

/// Output returned from a bash tool invocation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BashCommandOutput {
    pub stdout: String,
    pub stderr: String,
    #[serde(rename = "rawOutputPath")]
    pub raw_output_path: Option<String>,
    pub interrupted: bool,
    #[serde(rename = "isImage")]
    pub is_image: Option<bool>,
    #[serde(rename = "backgroundTaskId")]
    pub background_task_id: Option<String>,
    #[serde(rename = "backgroundedByUser")]
    pub backgrounded_by_user: Option<bool>,
    #[serde(rename = "assistantAutoBackgrounded")]
    pub assistant_auto_backgrounded: Option<bool>,
    #[serde(rename = "dangerouslyDisableSandbox")]
    pub dangerously_disable_sandbox: Option<bool>,
    #[serde(rename = "returnCodeInterpretation")]
    pub return_code_interpretation: Option<String>,
    #[serde(rename = "noOutputExpected")]
    pub no_output_expected: Option<bool>,
    #[serde(rename = "structuredContent")]
    pub structured_content: Option<Vec<serde_json::Value>>,
    #[serde(rename = "persistedOutputPath")]
    pub persisted_output_path: Option<String>,
    #[serde(rename = "persistedOutputSize")]
    pub persisted_output_size: Option<u64>,
    #[serde(rename = "sandboxStatus")]
    pub sandbox_status: Option<SandboxStatus>,
}

/// Executes a shell command with the requested sandbox settings.
pub fn execute_bash(input: BashCommandInput) -> io::Result<BashCommandOutput> {
    let cwd = env::current_dir()?;
    let sandbox_status = sandbox_status_for_input(&input, &cwd);

    if input.run_in_background.unwrap_or(false) {
        let mut child = prepare_command(&input.command, &cwd, &sandbox_status, false);
        let child = child
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        return Ok(BashCommandOutput {
            stdout: String::new(),
            stderr: String::new(),
            raw_output_path: None,
            interrupted: false,
            is_image: None,
            background_task_id: Some(child.id().to_string()),
            backgrounded_by_user: Some(false),
            assistant_auto_backgrounded: Some(false),
            dangerously_disable_sandbox: input.dangerously_disable_sandbox,
            return_code_interpretation: None,
            no_output_expected: Some(true),
            structured_content: None,
            persisted_output_path: None,
            persisted_output_size: None,
            sandbox_status: Some(sandbox_status),
        });
    }

    let runtime = Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(execute_bash_async(input, sandbox_status, cwd))
}

/// Detect git push to main and emit ship provenance event
fn detect_and_emit_ship_prepared(command: &str) {
    let trimmed = command.trim();
    // Simple detection: git push with main/master
    if trimmed.contains("git push") && (trimmed.contains("main") || trimmed.contains("master")) {
        // Emit ship.prepared event
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let provenance = ShipProvenance {
            source_branch: get_current_branch().unwrap_or_else(|| "unknown".to_string()),
            base_commit: get_head_commit().unwrap_or_default(),
            commit_count: 0, // Would need to calculate from range
            commit_range: "unknown..HEAD".to_string(),
            merge_method: ShipMergeMethod::DirectPush,
            actor: get_git_actor().unwrap_or_else(|| "unknown".to_string()),
            pr_number: None,
        };
        let _event = LaneEvent::ship_prepared(format!("{now}"), &provenance);
        // Log to stderr as interim routing before event stream integration
        eprintln!(
            "[ship.prepared] branch={} -> main, commits={}, actor={}",
            provenance.source_branch, provenance.commit_count, provenance.actor
        );
    }
}

fn get_current_branch() -> Option<String> {
    let output = Command::new("git")
        .args(["branch", "--show-current"])
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

fn get_head_commit() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

fn get_git_actor() -> Option<String> {
    let name = Command::new("git")
        .args(["config", "user.name"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())?;
    Some(name)
}

async fn execute_bash_async(
    input: BashCommandInput,
    sandbox_status: SandboxStatus,
    cwd: std::path::PathBuf,
) -> io::Result<BashCommandOutput> {
    // Detect and emit ship provenance for git push operations
    detect_and_emit_ship_prepared(&input.command);

    let mut command = prepare_tokio_command(&input.command, &cwd, &sandbox_status, true);

    let output_result = if let Some(timeout_ms) = input.timeout {
        if let Ok(result) = timeout(Duration::from_millis(timeout_ms), command.output()).await {
            (result?, false)
        } else {
            return Ok(timeout_output(&input, timeout_ms, sandbox_status));
        }
    } else {
        (command.output().await?, false)
    };

    let (output, interrupted) = output_result;
    let stdout = truncate_output(&String::from_utf8_lossy(&output.stdout));
    let stderr = truncate_output(&String::from_utf8_lossy(&output.stderr));
    let no_output_expected = Some(stdout.trim().is_empty() && stderr.trim().is_empty());
    let return_code_interpretation = output.status.code().and_then(|code| {
        if code == 0 {
            None
        } else {
            Some(format!("exit_code:{code}"))
        }
    });

    Ok(BashCommandOutput {
        stdout,
        stderr,
        raw_output_path: None,
        interrupted,
        is_image: None,
        background_task_id: None,
        backgrounded_by_user: None,
        assistant_auto_backgrounded: None,
        dangerously_disable_sandbox: input.dangerously_disable_sandbox,
        return_code_interpretation,
        no_output_expected,
        structured_content: None,
        persisted_output_path: None,
        persisted_output_size: None,
        sandbox_status: Some(sandbox_status),
    })
}

fn timeout_output(
    input: &BashCommandInput,
    timeout_ms: u64,
    sandbox_status: SandboxStatus,
) -> BashCommandOutput {
    let is_test = is_test_command(&input.command);
    let return_code_interpretation = if is_test { "test.hung" } else { "timeout" };
    BashCommandOutput {
        stdout: String::new(),
        stderr: format!("Command exceeded timeout of {timeout_ms} ms"),
        raw_output_path: None,
        interrupted: true,
        is_image: None,
        background_task_id: None,
        backgrounded_by_user: None,
        assistant_auto_backgrounded: None,
        dangerously_disable_sandbox: input.dangerously_disable_sandbox,
        return_code_interpretation: Some(String::from(return_code_interpretation)),
        no_output_expected: Some(true),
        structured_content: Some(vec![test_timeout_provenance(
            &input.command,
            timeout_ms,
            is_test,
        )]),
        persisted_output_path: None,
        persisted_output_size: None,
        sandbox_status: Some(sandbox_status),
    }
}

fn is_test_command(command: &str) -> bool {
    let normalized = command
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    normalized.contains("cargo test")
        || normalized.contains("cargo nextest")
        || normalized.contains("npm test")
        || normalized.contains("pnpm test")
        || normalized.contains("yarn test")
        || normalized.contains("pytest")
}

fn test_timeout_provenance(
    command: &str,
    timeout_ms: u64,
    classified_as_test_hang: bool,
) -> serde_json::Value {
    json!({
        "event": if classified_as_test_hang { "test.hung" } else { "command.timeout" },
        "failureClass": if classified_as_test_hang { "test_hang" } else { "timeout" },
        "data": {
            "command": command,
            "timeoutMs": timeout_ms,
            "provenance": "bash.timeout",
            "classification": if classified_as_test_hang { "test.hung" } else { "timeout" }
        }
    })
}

fn sandbox_status_for_input(input: &BashCommandInput, cwd: &std::path::Path) -> SandboxStatus {
    let config = ConfigLoader::default_for(cwd).load().map_or_else(
        |_| SandboxConfig::default(),
        |runtime_config| runtime_config.sandbox().clone(),
    );
    let request = config.resolve_request(
        input.dangerously_disable_sandbox.map(|disabled| !disabled),
        input.namespace_restrictions,
        input.isolate_network,
        input.filesystem_mode,
        input.allowed_mounts.clone(),
    );
    resolve_sandbox_status_for_request(&request, cwd)
}

/// Rewrite POSIX drive-letter paths (`/e/...`, `/E/...`) into the Windows
/// form (`E:/...`) that `cmd.exe` understands.  claw's `bash` tool on Windows
/// routes commands through `cmd /C` (see `prepare_command`), but the model
/// frequently emits Git Bash style paths like `cd /e/NW工程/... && php -l ...`
/// because the system prompt or conversation history contains bash examples.
/// cmd.exe rejects `/e/` with "The system cannot find the path specified"
/// before the actual tool command (e.g. `php`) is ever invoked — the operator
/// sees an apparent "php not in PATH" misdagnosis.
///
/// Scope is deliberately narrow: only `/X/` (where X is a single ASCII
/// letter) **at a command-token boundary** is rewritten — i.e. the `/X`
/// must be immediately preceded by whitespace, `;`, `&`, `|`, or string
/// start, AND followed by `/`.  The "followed by `/`" guard is essential:
/// without it, cmd.exe flags like `/b` (`dir /b`), `/s`, `/h` would be
/// misdetected as POSIX drive-letter paths and rewritten to `b:` — breaking
/// every dir/findstr/etc flag.  Other POSIX paths (`/tmp/`, `~/`, `/var/`)
/// are left untouched — those have no clean Windows equivalent and the
/// model should emit Windows paths for them.
fn rewrite_posix_drive_paths_for_windows(command: &str) -> String {
    let chars: Vec<char> = command.chars().collect();
    let mut out = String::with_capacity(command.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '/' && i + 2 < chars.len() {
            let letter = chars[i + 1];
            let is_letter = letter.is_ascii_alphabetic();
            // MUST be followed by `/` — not space, not end.  This prevents
            // misdetecting cmd.exe flags (`/b`, `/s`, `/h`) as drive paths.
            let next_is_slash = chars[i + 2] == '/';
            let at_token_boundary = i == 0 || matches!(chars[i - 1], ' ' | '\t' | ';' | '&' | '|');
            if is_letter && next_is_slash && at_token_boundary {
                out.push(letter);
                out.push(':');
                i += 2;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod drive_path_tests {
    use super::rewrite_posix_drive_paths_for_windows;

    #[test]
    fn rewrites_drive_prefix_slash_e() {
        assert_eq!(
            rewrite_posix_drive_paths_for_windows("cd /e/NW工程/资料库/html && php -l x.php"),
            "cd e:/NW工程/资料库/html && php -l x.php"
        );
    }

    #[test]
    fn rewrites_uppercase_drive_letter() {
        assert_eq!(
            rewrite_posix_drive_paths_for_windows("ls /C/Windows"),
            "ls C:/Windows"
        );
    }

    #[test]
    fn leaves_bare_drive_prefix_without_trailing_slash_untouched() {
        // `/e` alone (no trailing `/`) is ambiguous — could be a cmd.exe
        // flag.  We no longer rewrite this; only `/e/` is rewritten.
        assert_eq!(rewrite_posix_drive_paths_for_windows("cd /e"), "cd /e");
    }

    #[test]
    fn leaves_cmd_exe_flags_untouched() {
        // cmd.exe flags like `/b`, `/s`, `/h` must NOT be rewritten to `b:`
        // — this was the original failure mode that broke `dir /b`.
        assert_eq!(
            rewrite_posix_drive_paths_for_windows(r#"dir "E:\stuff" /b"#),
            r#"dir "E:\stuff" /b"#
        );
        assert_eq!(
            rewrite_posix_drive_paths_for_windows(r#"findstr /I /S "pattern" /b"#),
            r#"findstr /I /S "pattern" /b"#
        );
    }

    #[test]
    fn leaves_relative_posix_paths_untouched() {
        // No leading `/` before the letter → `/e/` inside `./e/` is not a drive path
        assert_eq!(
            rewrite_posix_drive_paths_for_windows("cat ./e/x.txt"),
            "cat ./e/x.txt"
        );
    }

    #[test]
    fn leaves_tmp_and_home_untouched() {
        // `/tmp/`, `~/`, `/var/` are not drive-letter paths — leave alone
        assert_eq!(
            rewrite_posix_drive_paths_for_windows("cat /tmp/x.txt && ls ~/y"),
            "cat /tmp/x.txt && ls ~/y"
        );
    }

    #[test]
    fn leaves_already_windows_paths_untouched() {
        // `E:/...` has no leading `/` → pattern doesn't match, no rewrite
        assert_eq!(
            rewrite_posix_drive_paths_for_windows("cd E:/NW工程/x && php -l y.php"),
            "cd E:/NW工程/x && php -l y.php"
        );
    }

    #[test]
    fn handles_multiple_drive_paths_in_one_command() {
        assert_eq!(
            rewrite_posix_drive_paths_for_windows("cp /e/src/x /e/dst/y"),
            "cp e:/src/x e:/dst/y"
        );
    }
}

fn prepare_command(
    command: &str,
    cwd: &std::path::Path,
    sandbox_status: &SandboxStatus,
    create_dirs: bool,
) -> Command {
    if create_dirs {
        prepare_sandbox_dirs(cwd);
    }

    if let Some(launcher) = build_linux_sandbox_command(command, cwd, sandbox_status) {
        let mut prepared = Command::new(launcher.program);
        prepared.args(launcher.args);
        prepared.current_dir(cwd);
        prepared.envs(launcher.env);
        return prepared;
    }

    // On Windows, use cmd.exe instead of sh.  Rewrite POSIX drive-letter
    // paths (/e/...) to Windows form (E:/...) first — the model frequently
    // emits Git Bash style paths that cmd.exe rejects.
    //
    // #186: feed `/C <command>` via `raw_arg` rather than two separate
    // `.arg()` calls.  Rust's `Command::arg` wraps each argv element in
    // quotes when it contains whitespace, which turns `cmd /C "dir \"E:\...\" /b"`
    // into a doubly-quoted string that cmd.exe's `/C` parsing interprets
    // as having escaped inner quotes — and the path comes out garbled,
    // reporting "The filename, directory name, or volume label syntax is
    // incorrect." even when the path is valid.  `raw_arg` skips Rust's
    // argv escaping and passes the bare command line to cmd.exe, which
    // then sees the command verbatim with inner quotes intact.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let rewritten = rewrite_posix_drive_paths_for_windows(command);

        let mut prepared = Command::new("cmd");
        prepared.raw_arg(format!("/C {rewritten}")).current_dir(cwd);
        if sandbox_status.filesystem_active {
            prepared.env("USERPROFILE", cwd.join(".sandbox-home"));
            prepared.env("TEMP", cwd.join(".sandbox-tmp"));
            prepared.env("TMP", cwd.join(".sandbox-tmp"));
        }
        prepared
    }

    // On Unix-like systems, use sh
    #[cfg(not(windows))]
    {
        let mut prepared = Command::new("sh");
        prepared.arg("-lc").arg(command).current_dir(cwd);
        if sandbox_status.filesystem_active {
            prepared.env("HOME", cwd.join(".sandbox-home"));
            prepared.env("TMPDIR", cwd.join(".sandbox-tmp"));
        }
        prepared
    }
}

fn prepare_tokio_command(
    command: &str,
    cwd: &std::path::Path,
    sandbox_status: &SandboxStatus,
    create_dirs: bool,
) -> TokioCommand {
    if create_dirs {
        prepare_sandbox_dirs(cwd);
    }

    if let Some(launcher) = build_linux_sandbox_command(command, cwd, sandbox_status) {
        let mut prepared = TokioCommand::new(launcher.program);
        prepared.args(launcher.args);
        prepared.current_dir(cwd);
        prepared.envs(launcher.env);
        return prepared;
    }

    // On Windows, use cmd.exe instead of sh.  Rewrite POSIX drive-letter
    // paths (/e/...) to Windows form (E:/...) first — see prepare_command
    // for the rationale.  Use `raw_arg` to skip Rust's argv quoting so
    // inner quotes in the command survive cmd.exe's `/C` parsing.
    #[cfg(windows)]
    {
        let rewritten = rewrite_posix_drive_paths_for_windows(command);

        let mut prepared = TokioCommand::new("cmd");
        prepared.raw_arg(format!("/C {rewritten}")).current_dir(cwd);
        if sandbox_status.filesystem_active {
            prepared.env("USERPROFILE", cwd.join(".sandbox-home"));
            prepared.env("TEMP", cwd.join(".sandbox-tmp"));
            prepared.env("TMP", cwd.join(".sandbox-tmp"));
        }
        prepared
    }

    // On Unix-like systems, use sh
    #[cfg(not(windows))]
    {
        let mut prepared = TokioCommand::new("sh");
        prepared.arg("-lc").arg(command).current_dir(cwd);
        if sandbox_status.filesystem_active {
            prepared.env("HOME", cwd.join(".sandbox-home"));
            prepared.env("TMPDIR", cwd.join(".sandbox-tmp"));
        }
        prepared
    }
}

fn prepare_sandbox_dirs(cwd: &std::path::Path) {
    let _ = std::fs::create_dir_all(cwd.join(".sandbox-home"));
    let _ = std::fs::create_dir_all(cwd.join(".sandbox-tmp"));
}

#[cfg(test)]
mod tests {
    use super::{execute_bash, BashCommandInput};
    use crate::sandbox::FilesystemIsolationMode;

    #[test]
    fn executes_simple_command() {
        let output = execute_bash(BashCommandInput {
            command: String::from("printf 'hello'"),
            timeout: Some(1_000),
            description: None,
            run_in_background: Some(false),
            dangerously_disable_sandbox: Some(false),
            namespace_restrictions: Some(false),
            isolate_network: Some(false),
            filesystem_mode: Some(FilesystemIsolationMode::WorkspaceOnly),
            allowed_mounts: None,
        })
        .expect("bash command should execute");

        assert_eq!(output.stdout, "hello");
        assert!(!output.interrupted);
        assert!(output.sandbox_status.is_some());
    }

    #[test]
    fn disables_sandbox_when_requested() {
        let output = execute_bash(BashCommandInput {
            command: String::from("printf 'hello'"),
            timeout: Some(1_000),
            description: None,
            run_in_background: Some(false),
            dangerously_disable_sandbox: Some(true),
            namespace_restrictions: None,
            isolate_network: None,
            filesystem_mode: None,
            allowed_mounts: None,
        })
        .expect("bash command should execute");

        assert!(!output.sandbox_status.expect("sandbox status").enabled);
    }

    #[test]
    fn timed_out_test_command_is_classified_as_hung_test_with_provenance() {
        let output = execute_bash(BashCommandInput {
            command: String::from("sleep 1 # cargo test slow_case"),
            timeout: Some(1),
            description: None,
            run_in_background: Some(false),
            dangerously_disable_sandbox: Some(false),
            namespace_restrictions: Some(false),
            isolate_network: Some(false),
            filesystem_mode: Some(FilesystemIsolationMode::WorkspaceOnly),
            allowed_mounts: None,
        })
        .expect("bash command should return structured timeout");

        assert!(output.interrupted);
        assert_eq!(
            output.return_code_interpretation.as_deref(),
            Some("test.hung")
        );
        let structured = output.structured_content.expect("structured content");
        assert_eq!(structured[0]["event"], "test.hung");
        assert_eq!(structured[0]["data"]["provenance"], "bash.timeout");
    }
}

/// Maximum output bytes before truncation (16 KiB, matching upstream).
const MAX_OUTPUT_BYTES: usize = 16_384;

/// Truncate output to `MAX_OUTPUT_BYTES`, appending a marker when trimmed.
fn truncate_output(s: &str) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s.to_string();
    }
    // Find the last valid UTF-8 boundary at or before MAX_OUTPUT_BYTES
    let mut end = MAX_OUTPUT_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = s[..end].to_string();
    truncated.push_str("\n\n[output truncated — exceeded 16384 bytes]");
    truncated
}

#[cfg(test)]
mod truncation_tests {
    use super::*;

    #[test]
    fn short_output_unchanged() {
        let s = "hello world";
        assert_eq!(truncate_output(s), s);
    }

    #[test]
    fn long_output_truncated() {
        let s = "x".repeat(20_000);
        let result = truncate_output(&s);
        assert!(result.len() < 20_000);
        assert!(result.ends_with("[output truncated — exceeded 16384 bytes]"));
    }

    #[test]
    fn exact_boundary_unchanged() {
        let s = "a".repeat(MAX_OUTPUT_BYTES);
        assert_eq!(truncate_output(&s), s);
    }

    #[test]
    fn one_over_boundary_truncated() {
        let s = "a".repeat(MAX_OUTPUT_BYTES + 1);
        let result = truncate_output(&s);
        assert!(result.contains("[output truncated"));
    }
}

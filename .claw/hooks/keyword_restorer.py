#!/usr/bin/env python3
"""
Keyword Restorer Hook for Claw Code (PreToolUse)

This hook restores redacted keywords before writing/editing files.
It reverses the replacements made by keyword_redactor.py.

For Edit tools, it restores BOTH old_string and new_string so that:
- old_string can match the actual file content
- new_string contains the correct original keywords

Usage:
- Add to .claude/settings.json as a PreToolUse hook for Write and Edit tools
- Edit keywords_config.py to define your own keywords and replacement patterns
"""

import sys
import json
import hashlib
import platform
import os
import time
import signal
import threading

# Import shared configuration
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from keywords_config import KEYWORDS, HASH_LENGTH

# Debug logging
DEBUG_LOG = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'hook_debug.log')

def log_debug(msg):
    """Write debug message to log file."""
    with open(DEBUG_LOG, 'a', encoding='utf-8') as f:
        f.write(f"[{time.strftime('%Y-%m-%d %H:%M:%S')}] {msg}\n")

# Windows command replacements (Unix -> Windows)
WINDOWS_CMD_REPLACEMENTS = {
    # file viewing
    "cat ": "type ",
    "cat\t": "type\t",
    # search
    "grep -r ": "findstr /S /I ",
    "grep ": "findstr /I ",
    "grep\t": "findstr /I\t",
    # list files
    "ls ": "dir ",
    "ls\t": "dir\t",
    "ls": "dir",
    # find files (recursive, wildcard)
    "find ": "dir /s /b ",
    # remove
    "rm ": "del ",
    "rm\t": "del\t",
    # move/rename
    "mv ": "move ",
    "mv\t": "move\t",
    # copy
    "cp ": "copy ",
    "cp\t": "copy\t",
    # clear screen
    "clear": "cls",
}

# ============================================================
# IMPLEMENTATION
# ============================================================

def generate_hash(keyword: str) -> str:
    """Generate the same deterministic hash as keyword_redactor.py"""
    return hashlib.sha256(keyword.encode()).hexdigest()[:HASH_LENGTH]


def restore_keywords(content: str) -> str:
    """Restore all redacted keywords back to originals."""
    result = content
    for keyword, pattern in KEYWORDS.items():
        hash_value = generate_hash(keyword)
        replacement = pattern.replace("{hash}", hash_value)
        if replacement in result:
            result = result.replace(replacement, keyword)
    return result


def convert_unix_cmd_to_windows(command: str) -> str:
    """Convert Unix commands to Windows equivalents on Windows platform."""
    if platform.system() != "Windows":
        return command

    # Iterate by precedence: longer Unix prefixes (e.g. "grep -r ") must be
    # checked before shorter ones (e.g. "grep ") so a command like
    # `grep -r foo bar` is converted to `findstr /S /I foo bar`, not
    # `findstr /I -r foo bar`.
    sorted_items = sorted(
        WINDOWS_CMD_REPLACEMENTS.items(),
        key=lambda kv: len(kv[0]),
        reverse=True,
    )

    # Special handling for `grep -<flags>` variants: collapse any
    # `grep -rn` / `grep -rl` / `grep -rF` etc. to `findstr /S /I`,
    # stripping the Unix flag cluster.  Without this, only the exact
    # prefix `grep -r ` matches and `grep -rn foo bar` would leave `-rn`
    # dangling into the findstr call.
    def convert_one(token: str) -> str:
        # `grep -<flags> ...` → `findstr /S /I ...` (flags stripped)
        # greedy: matches `grep -rn`, `grep -rl`, `grep -rF`, etc.
        import re
        m = re.match(r'^grep -[A-Za-z]+ ', token)
        if m:
            return 'findstr /S /I ' + token[m.end():]
        return None

    result = command
    for unix_cmd, windows_cmd in sorted_items:
        # Replace at the beginning of the command
        if result.startswith(unix_cmd):
            # Special: `grep -<flags>` variant
            converted = convert_one(result)
            if converted is not None:
                result = converted
            else:
                result = windows_cmd + result[len(unix_cmd):]
            break
        # Replace after a separator: | && || ;  (the model often emits
        # `cd /path && ls ...` — without this branch ls never converts)
        for sep in ("| ", "&& ", "|| ", "; "):
            needle = sep + unix_cmd
            if needle in result:
                # Special: `grep -<flags>` variant after separator.
                # Pass the tail *after* the separator (which starts with
                # the Unix cmd) so convert_one's `^grep -` regex matches.
                tail = result[result.find(needle) + len(sep):]
                converted_tail = convert_one(tail)
                if converted_tail is not None:
                    result = (
                        result[:result.find(needle) + len(sep)]
                        + converted_tail
                    )
                else:
                    result = result.replace(needle, sep + windows_cmd, 1)
                break
        else:
            continue
        break

    return result


def timeout_handler(signum, frame):
    """Handle timeout - exit gracefully."""
    log_debug("Timeout occurred")
    sys.stdout.write('{}\n')
    sys.stdout.flush()
    sys.exit(0)


def write_output_with_timeout(output_json, timeout_seconds=60):
    """Write output to stdout with a timeout to prevent hanging."""
    log_debug(f"Writing output ({len(output_json)} chars)")

    # Use a thread to write with timeout
    success = [False]
    error_msg = [None]

    def write_thread():
        try:
            # Write in very small chunks with delays for large outputs
            if len(output_json) > 50000:
                chunk_size = 4096  # 4KB chunks for large outputs
                for i in range(0, len(output_json), chunk_size):
                    chunk = output_json[i:i+chunk_size]
                    sys.stdout.write(chunk)
                    sys.stdout.flush()
                    time.sleep(0.01)  # 10ms delay between chunks
            else:
                sys.stdout.write(output_json)

            sys.stdout.write('\n')
            sys.stdout.flush()
            success[0] = True
            log_debug("Write completed")
        except Exception as e:
            error_msg[0] = str(e)
            log_debug(f"Write thread error: {e}")

    thread = threading.Thread(target=write_thread)
    thread.daemon = True
    thread.start()
    thread.join(timeout_seconds)

    if not success[0]:
        log_debug(f"Write timeout or failure: {error_msg[0]}")
        # Return empty JSON as fallback
        try:
            sys.stdout.write('{}\n')
            sys.stdout.flush()
        except:
            pass


def is_accessing_hook_files(tool_input: dict, tool_name: str) -> bool:
    """Check if the tool is trying to access hook scripts or config files."""
    protected_files = [
        "keyword_redactor.py",
        "keyword_restorer.py",
        "keywords_config.py",
        "hook_debug.log",
    ]

    if tool_name in ("Read", "read_file"):
        file_path = tool_input.get("file_path", "")
        normalized = file_path.replace("\\", "/").lower()
        for protected in protected_files:
            if protected in normalized and "hooks/" in normalized:
                return True
    elif tool_name in ("bash", "Bash", "PowerShell"):
        command = tool_input.get("command", "")
        command_lower = command.lower()
        for protected in protected_files:
            if protected in command_lower:
                return True
        if "hooks/" in command_lower and ("cat" in command_lower or "type" in command_lower or "open(" in command_lower):
            return True
    elif tool_name in ("Write", "write_file"):
        content = tool_input.get("content", "")
        content_lower = content.lower()
        for protected in protected_files:
            if protected in content_lower:
                return True
    return False


def main():
    try:
        # Set timeout to 30 seconds for large files
        if hasattr(signal, 'SIGALRM'):
            signal.signal(signal.SIGALRM, timeout_handler)
            signal.alarm(30)

        log_debug("Restorer hook started")

        # Read JSON input from stdin
        log_debug("Waiting for stdin...")
        input_data = json.load(sys.stdin)
        log_debug(f"Received input, tool: {input_data.get('tool_name', 'unknown')}")

        # Extract tool information
        tool_name = input_data.get("tool_name", "")
        tool_input = input_data.get("tool_input", {})
        log_debug(f"Processing tool: {tool_name}")

        # Security check: prevent accessing hook files
        if is_accessing_hook_files(tool_input, tool_name):
            log_debug("Blocked: accessing hook files")
            write_output_with_timeout('{}', timeout_seconds=5)
            sys.exit(0)

        # Process based on tool type
        if tool_name in ("Write", "write_file"):
            log_debug("Processing Write tool")
            content = tool_input.get("content", "")
            restored_content = restore_keywords(content)

            if restored_content != content:
                write_output_with_timeout(json.dumps({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "allow",
                        "updatedInput": {
                            **tool_input,
                            "content": restored_content
                        }
                    }
                }), timeout_seconds=10)
            else:
                write_output_with_timeout(json.dumps({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "allow"
                    }
                }), timeout_seconds=5)

        elif tool_name in ("Edit", "edit_file"):
            log_debug("Processing Edit tool")
            # For Edit tool, we need to restore BOTH old_string and new_string
            old_string = tool_input.get("old_string", "")
            new_string = tool_input.get("new_string", "")

            restored_old = restore_keywords(old_string)
            restored_new = restore_keywords(new_string)

            # Check if any changes were made
            if restored_old != old_string or restored_new != new_string:
                write_output_with_timeout(json.dumps({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "allow",
                        "updatedInput": {
                            **tool_input,
                            "old_string": restored_old,
                            "new_string": restored_new
                        }
                    }
                }), timeout_seconds=10)
            else:
                write_output_with_timeout(json.dumps({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "allow"
                    }
                }), timeout_seconds=5)

        elif tool_name in ("Bash", "bash", "PowerShell"):
            log_debug("Processing Bash tool")
            # For Bash/PowerShell, restore keywords and convert commands
            command = tool_input.get("command", "")
            restored_command = restore_keywords(command)
            # Convert Unix commands to Windows equivalents
            restored_command = convert_unix_cmd_to_windows(restored_command)

            if restored_command != command:
                write_output_with_timeout(json.dumps({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "allow",
                        "updatedInput": {
                            **tool_input,
                            "command": restored_command
                        }
                    }
                }), timeout_seconds=10)
            else:
                write_output_with_timeout(json.dumps({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "allow"
                    }
                }), timeout_seconds=5)

        elif tool_name in ("NotebookEdit", "notebook_edit"):
            log_debug("Processing NotebookEdit tool")
            # For NotebookEdit, restore keywords in new_source
            new_source = tool_input.get("new_source", "")
            restored_source = restore_keywords(new_source)

            if restored_source != new_source:
                write_output_with_timeout(json.dumps({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "allow",
                        "updatedInput": {
                            **tool_input,
                            "new_source": restored_source
                        }
                    }
                }), timeout_seconds=10)
            else:
                write_output_with_timeout(json.dumps({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "allow"
                    }
                }), timeout_seconds=5)

        else:
            log_debug(f"Pass through for tool: {tool_name}")
            # For other tools, pass through without modification
            write_output_with_timeout('{}', timeout_seconds=5)
            sys.exit(0)

        # Cancel the alarm
        if hasattr(signal, 'SIGALRM'):
            signal.alarm(0)
        sys.exit(0)

    except json.JSONDecodeError as e:
        log_debug(f"JSON error: {e}")
        write_output_with_timeout(json.dumps({"error": f"Invalid JSON input: {e}"}), timeout_seconds=5)
        sys.exit(1)
    except Exception as e:
        log_debug(f"Error: {e}")
        write_output_with_timeout(json.dumps({"error": str(e)}), timeout_seconds=5)
        sys.exit(1)


if __name__ == "__main__":
    main()

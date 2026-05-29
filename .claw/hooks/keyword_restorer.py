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

# Import shared configuration
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from keywords_config import KEYWORDS, HASH_LENGTH

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

    result = command
    for unix_cmd, windows_cmd in WINDOWS_CMD_REPLACEMENTS.items():
        # Only replace at the beginning of command or after pipe/semicolon
        # Simple approach: replace if command starts with unix_cmd
        if result.startswith(unix_cmd):
            result = windows_cmd + result[len(unix_cmd):]
            break
        # Also handle commands after pipe
        if "| " + unix_cmd in result:
            result = result.replace("| " + unix_cmd, "| " + windows_cmd, 1)
            break

    return result


def main():
    try:
        # Read JSON input from stdin
        input_data = json.load(sys.stdin)

        # Extract tool information
        tool_name = input_data.get("tool_name", "")
        tool_input = input_data.get("tool_input", {})

        # Process based on tool type
        if tool_name in ("Write", "write_file"):
            content = tool_input.get("content", "")
            restored_content = restore_keywords(content)

            if restored_content != content:
                print(json.dumps({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "allow",
                        "updatedInput": {
                            **tool_input,
                            "content": restored_content
                        }
                    }
                }))
            else:
                print(json.dumps({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "allow"
                    }
                }))

        elif tool_name in ("Edit", "edit_file"):
            # For Edit tool, we need to restore BOTH old_string and new_string
            old_string = tool_input.get("old_string", "")
            new_string = tool_input.get("new_string", "")

            restored_old = restore_keywords(old_string)
            restored_new = restore_keywords(new_string)

            # Check if any changes were made
            if restored_old != old_string or restored_new != new_string:
                print(json.dumps({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "allow",
                        "updatedInput": {
                            **tool_input,
                            "old_string": restored_old,
                            "new_string": restored_new
                        }
                    }
                }))
            else:
                print(json.dumps({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "allow"
                    }
                }))

        elif tool_name in ("Bash", "bash", "PowerShell"):
            # For Bash/PowerShell, restore keywords and convert commands
            command = tool_input.get("command", "")
            restored_command = restore_keywords(command)
            # Convert Unix commands to Windows equivalents
            restored_command = convert_unix_cmd_to_windows(restored_command)

            if restored_command != command:
                print(json.dumps({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "allow",
                        "updatedInput": {
                            **tool_input,
                            "command": restored_command
                        }
                    }
                }))
            else:
                print(json.dumps({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "allow"
                    }
                }))

        elif tool_name in ("NotebookEdit", "notebook_edit"):
            # For NotebookEdit, restore keywords in new_source
            new_source = tool_input.get("new_source", "")
            restored_source = restore_keywords(new_source)

            if restored_source != new_source:
                print(json.dumps({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "allow",
                        "updatedInput": {
                            **tool_input,
                            "new_source": restored_source
                        }
                    }
                }))
            else:
                print(json.dumps({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "allow"
                    }
                }))

        else:
            # For other tools, pass through without modification
            print(json.dumps({}))
            sys.exit(0)

        sys.exit(0)

    except json.JSONDecodeError as e:
        print(json.dumps({"error": f"Invalid JSON input: {e}"}), file=sys.stderr)
        sys.exit(1)
    except Exception as e:
        print(json.dumps({"error": str(e)}), file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()

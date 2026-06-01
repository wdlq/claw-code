#!/usr/bin/env python3
"""
Keyword Redactor Hook for Claw Code

This hook replaces sensitive keywords in file content before Claude sees them.
The replacement is deterministic - the same keyword always produces the same replacement.

Usage:
- Add to .claude/settings.json to activate
- Edit keywords_config.py to define your own keywords and replacement patterns
"""

import sys
import json
import hashlib
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

# ============================================================
# IMPLEMENTATION (no need to modify below this line)
# ============================================================

def generate_hash(keyword: str) -> str:
    """
    Generate a deterministic hash for a keyword.
    Same keyword always produces the same hash.
    """
    return hashlib.sha256(keyword.encode()).hexdigest()[:HASH_LENGTH]


def replace_keywords(content: str) -> str:
    """Replace all configured keywords in the content."""
    result = content
    for keyword, pattern in KEYWORDS.items():
        if keyword in result:
            hash_value = generate_hash(keyword)
            replacement = pattern.replace("{hash}", hash_value)
            result = result.replace(keyword, replacement)
    return result


def is_reading_hook_self(tool_input: dict, tool_name: str) -> bool:
    """Check if the tool is reading hook scripts or config files."""
    # Files to protect from reading
    protected_files = [
        "hooks/keyword_redactor.py",
        "hooks/keyword_restorer.py",
        "hooks/keywords_config.py",
        "hooks/hook_debug.log",
    ]

    if tool_name == "Read":
        file_path = tool_input.get("file_path", "")
        # Normalize path separators for cross-platform comparison
        normalized = file_path.replace("\\", "/").lower()
        for protected in protected_files:
            if protected in normalized:
                return True
    elif tool_name == "Grep":
        path = tool_input.get("path", "")
        pattern = tool_input.get("pattern", "")
        normalized_path = path.replace("\\", "/").lower()
        # Block grep that targets hook files or searches for keyword patterns
        if "hooks/" in normalized_path and ("keyword_redactor" in normalized_path or "keyword_restorer" in normalized_path or "keywords_config" in normalized_path):
            return True
        # Block grep patterns that look like they're trying to extract KEYWORDS dict
        if "KEYWORDS" in pattern and "hooks" in normalized_path:
            return True
    elif tool_name in ("bash", "PowerShell"):
        # Block commands that try to read hook files
        command = tool_input.get("command", "")
        command_lower = command.lower()
        # Check for direct file reading commands
        for protected in protected_files:
            if protected.replace("hooks/", "") in command_lower:
                return True
        # Check for Python scripts that might read hook files
        if "keywords_config" in command_lower or "keyword_redactor" in command_lower or "keyword_restorer" in command_lower:
            return True
        # Check for commands that try to list or access hooks directory
        if "hooks/" in command_lower and ("cat" in command_lower or "type" in command_lower or "get-content" in command_lower or "open(" in command_lower):
            return True
    elif tool_name == "write_file":
        # Block writing scripts that reference hook files
        content = tool_input.get("content", "")
        content_lower = content.lower()
        if "keywords_config" in content_lower or "keyword_redactor" in content_lower or "keyword_restorer" in content_lower:
            return True
        if "hooks/" in content_lower and ("open(" in content_lower or "read" in content_lower):
            return True
    return False


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


def main():
    try:
        # Set timeout to 30 seconds for large files
        if hasattr(signal, 'SIGALRM'):
            signal.signal(signal.SIGALRM, timeout_handler)
            signal.alarm(30)

        log_debug("Hook started")

        # Read JSON input from stdin
        log_debug("Waiting for stdin...")
        input_data = json.load(sys.stdin)
        log_debug(f"Received input, tool: {input_data.get('tool_name', 'unknown')}")

        # Extract tool information
        tool_name = input_data.get("tool_name", "")
        tool_input = input_data.get("tool_input", {})
        tool_output = input_data.get("tool_output", "")

        log_debug(f"Tool: {tool_name}, output length: {len(str(tool_output))}")

        # Security check: prevent reading this hook script itself
        if is_reading_hook_self(tool_input, tool_name):
            log_debug("Blocked: reading hook self")
            print(json.dumps({}))
            sys.exit(0)

        # Only process if there's actual content
        if not tool_output:
            log_debug("No output to process")
            sys.stdout.write('{}\n')
            sys.stdout.flush()
            sys.exit(0)

        # Replace keywords in the output
        log_debug("Replacing keywords...")
        modified_output = replace_keywords(tool_output)
        log_debug("Keywords replaced")

        # Only return updated output if something changed
        if modified_output != tool_output:
            result = {
                "hookSpecificOutput": {
                    "hookEventName": "PostToolUse",
                    "updatedToolOutput": modified_output
                }
            }
            log_debug("Output modified, returning result")
            # Use timeout-safe write for large outputs
            output_json = json.dumps(result)
            write_output_with_timeout(output_json, timeout_seconds=10)
        else:
            # No changes needed, return empty JSON
            log_debug("No changes needed")
            write_output_with_timeout('{}', timeout_seconds=5)

        log_debug("Hook completed")
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

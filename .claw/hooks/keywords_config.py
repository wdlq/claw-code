"""
Shared keyword configuration for keyword_redactor.py and keyword_restorer.py

Edit this file to define your sensitive keywords and replacement patterns.
Both hooks will import from this single source of truth.
"""

# Format: "keyword": "replacement_pattern"
# Use {hash} in pattern to insert a deterministic hash value
KEYWORDS = {
    "派驻": "呆_{hash}",
    "介绍": "绍介_{hash}",
    "组": "房子_{hash}",
    "派出": "出行_{hash}"
}

# Hash length to use (shorter = more compact output)
HASH_LENGTH = 8

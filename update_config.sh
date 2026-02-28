#!/bin/bash
cat > state/config.toml << 'INNER_EOF'
api_key = "***REMOVED-PROXY-KEY***"
default_provider = "anthropic-custom:https://a1.devku.ai"
default_model = "claude-sonnet-4-6"
default_temperature = 0.7
model_routes = []
embedding_routes = []

[observability]
backend = "none"
runtime_trace_mode = "none"
runtime_trace_path = "state/runtime-trace.jsonl"
runtime_trace_max_entries = 200

[autonomy]
level = "supervised"
workspace_only = true
allowed_commands = [
    "git",
    "cargo",
    "npm",
    "python",
    "ls",
    "cat",
]
INNER_EOF

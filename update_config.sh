#!/bin/bash
cat > state/config.toml << 'INNER_EOF'
api_key = "sk-2c873d1ca8c9109de6857a19b1dd3a314fe4b069cec776398c47144bc669328b"
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

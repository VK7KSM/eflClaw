#!/bin/bash
curl -s -X POST https://a1.devku.ai/v1/messages \
  -H "x-api-key: sk-2c873d1ca8c9109de6857a19b1dd3a314fe4b069cec776398c47144bc669328b" \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d '{
    "model": "claude-sonnet-4-6",
    "max_tokens": 10,
    "messages": [
      {"role": "user", "content": "Hello"}
    ]
  }'

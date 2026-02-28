#!/bin/bash
curl -s -X POST https://a1.devku.ai/v1/chat/completions \
  -H "Authorization: Bearer sk-2c873d1ca8c9109de6857a19b1dd3a314fe4b069cec776398c47144bc669328b" \
  -H "content-type: application/json" \
  -d '{
    "model": "claude-sonnet-4-6",
    "messages": [
      {"role": "user", "content": "Hello"}
    ]
  }'

#!/bin/bash
curl -s -X POST https://a1.devku.ai/v1/chat/completions \
  -H "Authorization: Bearer ***REMOVED-PROXY-KEY***" \
  -H "content-type: application/json" \
  -d '{
    "model": "claude-sonnet-4-6",
    "messages": [
      {"role": "user", "content": "Hello"}
    ]
  }'

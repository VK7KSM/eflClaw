#!/bin/bash
curl -s -X POST https://a1.devku.ai/v1/messages \
  -H "x-api-key: ***REMOVED-PROXY-KEY***" \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d '{
    "model": "claude-sonnet-4-6",
    "max_tokens": 10,
    "messages": [
      {"role": "user", "content": "Hello"}
    ]
  }'

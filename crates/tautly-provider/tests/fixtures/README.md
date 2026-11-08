# Test fixtures

The files under `anthropic/` are hand-authored from Anthropic's documented Messages
streaming format (event types `message_start`, `content_block_start`,
`content_block_delta`, `content_block_stop`, `message_delta`, `message_stop`, `ping`,
`error`). They are NOT recordings of real API responses, because no API keys were
available when they were written.

They must be replaced by real recordings (with keys scrubbed) when API keys are
available.

The files under `openai/` are likewise hand-authored, from OpenAI's documented Responses
streaming events (`response.created`, `response.output_item.added`/`.done`,
`response.output_text.delta`, `response.reasoning_summary_text.delta`,
`response.function_call_arguments.delta`/`.done`, `response.completed`,
`response.incomplete`, `response.failed`, `error`). They are NOT recordings of real API
responses, and must be replaced by real recordings (keys scrubbed) when API keys are
available.

The files under `google/` are hand-authored, from Google's documented
`streamGenerateContent?alt=sse` chunks (`candidates[].content.parts[]` with `text`,
`thought`, `functionCall`; `finishReason`; `usageMetadata` including
`cachedContentTokenCount` and `thoughtsTokenCount`; an in-stream `error` object). The files
under `compat/` are hand-authored, from the OpenAI Chat Completions streaming format
(`choices[].delta` with `content`, `tool_calls[]` by `index`, `reasoning`/`reasoning_content`
as emitted by Ollama and DeepSeek-style servers, `finish_reason`, a trailing `usage` chunk,
`[DONE]`); `compat/ollama.sse` imitates Ollama's `/v1/chat/completions` output. None are
recordings of real responses, and all must be replaced by real recordings (keys scrubbed)
when API keys / a local server are available.

`compat/tool_calls_no_index.sse` is likewise hand-authored: tool-call deltas without an `index`
field, as emitted by some servers.

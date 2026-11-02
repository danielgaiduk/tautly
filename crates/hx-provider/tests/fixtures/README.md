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

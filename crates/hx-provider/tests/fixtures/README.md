# Test fixtures

The files under `anthropic/` are hand-authored from Anthropic's documented Messages
streaming format (event types `message_start`, `content_block_start`,
`content_block_delta`, `content_block_stop`, `message_delta`, `message_stop`, `ping`,
`error`). They are NOT recordings of real API responses, because no API keys were
available when they were written.

They must be replaced by real recordings (with keys scrubbed) when API keys are
available.

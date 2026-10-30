GitHub review webhook payloads from `octokit/webhooks`'s documented examples:

- `payload-examples/api.github.com/pull_request_review/submitted.payload.json`
- `payload-examples/api.github.com/pull_request_review_comment/created.payload.json`
- `payload-examples/api.github.com/pull_request_review_thread/resolved.payload.json`

Fetched on 2026-10-29. Title, body, description and message prose is replaced
with synthetic text. The older comment example is supplied with `draft: false`,
as in the other examples and the current simple pull-request schema. The simple pull-request object has `merged_at` and omits
`merged`; the tests exercise both null and set timestamps.

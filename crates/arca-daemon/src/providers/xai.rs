// xAI usage provider — DEFERRED.
//
// As of 2026-05-22, xAI's public API (docs.x.ai) does not expose an endpoint
// for retrieving account credit balance or usage. Confirmed against
// https://docs.x.ai/docs/api-reference — only chat/completions and responses
// endpoints are documented.
//
// Until that changes, track xAI spend via `manual.snapshot` against a
// synthetic account named "xAI API". See docs/providers.md.
//
// Module kept as a placeholder so the registry, tests, and future enablement
// can wire through here without churn elsewhere.

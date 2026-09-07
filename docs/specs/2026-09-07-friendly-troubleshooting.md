# Friendly troubleshooting and support uploads

Base: origin/main 01d2ce9 (1.0.4), fetched before creating this worktree.

The Settings troubleshooter should be understandable without networking or
media-stack knowledge. Show seven short, friendly items with green success or
red attention indicators and text labels. A check that could not finish must
say so, rather than inventing a cause. Do not display plugin names, codecs,
STUN/ICE/TURN, build hashes, timestamps, or raw historical diagnostics here.
Retain technical results for the support report. Use a short suggestion to try
a stream with a friend rather than claiming universal readiness.

After checking, offer **Send report**. One deliberate click sends the technical
report and bounded recent diagnostic log tails to the Orange team. Show
Sending, Sent, and a friendly retryable error. No automatic uploads. State
briefly that this sends the check results and recent Orange logs. Reuse signed-in
relay authentication; never embed storage credentials or session tokens in the
report. Keep Copy report as an offline fallback without displaying its contents.

## Upload contract

`POST /diagnostics` on the configured relay's HTTP(S) origin, bearer auth.

Request JSON:
```json
{"schema":1,"report":"full technical report","logs":[{"name":"orange-media-123.jsonl","contents":"JSONL tail","truncated":true}]}
```

- Maximum request 2 MiB; report at most 32 KiB; at most three log attachments,
  each at most 128 KiB UTF-8. Filenames are fixed diagnostic PID names only.
- Client strips sensitive metadata/credentials and skips malformed log records.
  Logs are optional: users without sessions can still send the check report.
- `truncated` means source records were omitted by tail limits or invalid/
  unsupported records. Privacy filtering of fields alone does not set it.
  A readable log is still attached if another file cannot be read.
- Successful response is HTTP 201 with `{"report_id":"<32 lowercase hex>"}`.
  Report IDs are receipt metadata, not a public download capability.
- Auth failure 401; invalid submission 400; oversized body 413; rate/concurrency
  limits 429; unconfigured/unavailable storage 503. Never return raw errors.
- Relay writes a JSON envelope with received time and a hash of the verified
  account ID to a private Azure Blob container. No public report retrieval route.
- Reuse ORANGE_TABLE_ACCOUNT/ORANGE_TABLE_KEY on the server; enable uploads with
  ORANGE_DIAGNOSTICS_CONTAINER. Deployment creates/configures a private
  `diagnostics` container in the existing storage account.
- Bound body read and storage timeouts, four concurrent requests, three uploads
  per minute/account and a bounded expiring limiter map. Client owns and reaps
  its bounded upload worker, suppresses stale results after sign-out or rerun.

Validate local HTTP success/failure/auth/size/rate paths, Azure request signing,
log bounds/redaction, cancellation and user-facing state mappings; then native
Settings appearance and interaction, Rust gates and deployment contract tests.

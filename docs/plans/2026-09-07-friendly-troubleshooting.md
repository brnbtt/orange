# Friendly troubleshooting implementation

Spec: `docs/specs/2026-09-07-friendly-troubleshooting.md`.

Baseline: serial workspace tests passed on origin/main 01d2ce9:
223 media, 134 desktop, 66 signal, nine updater (five ignored).

1. Server worker: add authenticated bounded POST /diagnostics and private Blob
   persistence using existing REST/HMAC dependencies. Update deployment and its
   contract tests. Test failure behavior and request signing before finalizing.
2. Client worker: replace technical Settings content with friendly green/red
   items; keep technical report internal. Add owned explicit-upload state and
   HTTP client, plain status/retry messages and existing authentication wiring.
3. Parent: collect bounded, sanitized recent JSONL tails in a focused client
   module, with redaction/partial-file/resource-limit regression tests.
4. Integrate and review both boundaries; verify native UI, local client/relay
   upload, all-feature workspace tests, clippy, formatting and PowerShell
   deployment/packaging contracts. Document deployment needs and usage.

Keep all storage credentials on the server. Do not add archive/SDK dependencies
or show raw diagnostic details in the user-facing results.

## Verification record

- All-feature serial workspace tests: **464 passed**, five ignored.
- All-target/all-feature Clippy with warnings denied, formatting check and
  workspace development build passed.
- All four PowerShell contracts passed (installer, package provenance, beta
  publisher and Azure deployment). Each ran in a separate PowerShell process.
- Native 720 x 990 Settings fixture verified friendly results and dots, explicit
  Send report, sent confirmation, 503 failure then retry, duplicate suppression,
  and responsive cancellation with Stopping feedback until the worker drains.
- The captured HTTP body contained the full technical report and a sanitized
  diagnostic attachment; synthetic token/address fields were absent. No request
  was sent before the explicit click. Server tests independently validated auth,
  quotas, body/time bounds and persistence before returning a receipt, using a
  local fake Blob service and independently checked Shared Key signing input.
- A locked old log no longer discards readable attachments. Small all-recognized
  logs remain `truncated: false`; the flag also describes omitted malformed or
  unsupported source records, not privacy filtering of individual fields.
- Cloud deployment and live Azure uploads were not performed. `deploy/azure.ps1`
  provisions the private container and enables the endpoint when this change is
  deployed; the desktop changes need a new client release.

## Deployment verification (subsequent release request)

Server commit `a93c96f` deployed successfully through ACR run `cqg` to ready
revision `orange-relay--0000020`, with one minimum/maximum replica and the
private `diagnostics` container enabled. A live unauthenticated POST returned
401. An authenticated synthetic report returned 201; the exact report and log
were downloaded from Blob storage and compared with the submitted payload.
Anonymous access to that object was denied, and the synthetic object was
deleted after verification. No real session log contents were uploaded by
this deployment check.

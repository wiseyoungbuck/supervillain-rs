# Inbox precache to 150 + Playwright E2E performance suite

**Date:** 2026-07-06
**Status:** Approved (design discussed in-session)

## Problem

Some inboxes take a long time to open or load. Root causes identified in the
running system:

1. Email **bodies** are only prefetched for the top 25 messages per mailbox
   (`BODY_PREFETCH_PER_MAILBOX = 25` in `src/prefetch.rs`). Opening anything
   older hits the provider live (300 ms–3 s+).
2. Newly authorized accounts stay cold until the next 5-minute warmer cycle —
   up to 5 minutes of slow loads right after setup.
3. There is no automated check that the precache behavior works; regressions
   surface only as "feels slow".

Inbox **lists** are already warmed at 150 (`DEFAULT_INBOX_LIMIT = 150`), so
this work is about bodies, warm timing, and tests.

## Requirement (user)

At least the **top 150 emails of each account's Inbox** must be precached
locally so opening them is fast. Other mailboxes may stay at the smaller
prefetch depth. Tests must run against the live local server at
`127.0.0.1:8000` with real accounts (no mock provider).

## Design

### 1. Backend precache changes (`src/prefetch.rs`)

- Add `BODY_PREFETCH_INBOX: usize = 150`. In the warmer's body fan-out
  (phase 2 of `warm_all_mailboxes`), mailboxes with `role == "inbox"` take
  150 ids; all other mailboxes keep `BODY_PREFETCH_PER_MAILBOX = 25`.
  The role is carried through phase 1's `warmed_ids` tuples.
- **Skip-if-cached:** before `fetch_bodies`, filter the id prefix to ids not
  already in `body_cache`. Bodies are immutable content — flags/keywords are
  refreshed by the list warm, and mutation paths already invalidate list
  caches while deliberately preserving `body_cache`. Without this filter,
  going to 150 would re-download ~600 mostly-unchanged bodies every 5
  minutes (Gmail quota risk called out in the existing const's comment);
  with it, steady-state cycles fetch only newly arrived mail.
- **Warm-on-authorize:** after a session is installed by
  `POST /api/accounts/{id}/authorize` (and after a new Fastmail account is
  created via upsert), spawn a one-shot warm task for that account
  immediately instead of waiting for the next 5-minute cycle.
- Memory cost: ~125 extra bodies × ~50 KB × connected accounts ≈ +6 MB per
  account. Acceptable for a local single-user app.

Pure-helper tests (repo discipline: pure helpers inline, no HTTP mocking):

- take-count selection by mailbox role (inbox → 150, other/none → 25),
- the cache-skip filter (already-cached ids excluded, order preserved),
- a source-level or unit check that authorize's session-install path
  triggers a warm.

### 2. Playwright suite (`e2e/`)

New top-level `e2e/` directory, isolated from cargo:

- `package.json` with `@playwright/test`; `playwright.config.ts` with
  `baseURL: http://127.0.0.1:8000`, single Chromium project,
  `reuseExistingServer: true` (the user's server runs as a service; CI is a
  non-goal).
- Account discovery: a fixture fetches `/api/accounts` and parameterizes
  tests over accounts with `authStatus === "ok"`; pending accounts are
  skipped, not failed.
- A **warmup fixture** does one untimed pass (visit each account's inbox)
  so timed assertions measure steady-state cache, not a cold server start.

Tests, per connected account:

| Test | Behavior asserted | Threshold |
|---|---|---|
| inbox open | switching to the account renders email rows | ≤ 2 s |
| shallow body | opening a top-of-list email renders the body pane | ≤ 1 s UI |
| deep body | opening an email around row 100–140 renders the body pane | ≤ 1 s UI |
| cached body API | `GET /api/email` for shallow + deep ids | ≤ 300 ms |
| mailbox switch | Inbox → Archive → Inbox re-renders list | ≤ 2 s each |

The 300 ms API threshold is the cache-hit detector: warmed responses are
<50 ms; live provider fetches are 300 ms–3 s. UI thresholds are looser to
absorb rendering variance and avoid flakes.

### 3. Wiring

- `CLAUDE.md` and README document the run command:
  `cd e2e && npm install && npx playwright test`.
- Not wired into `cargo test` — it is a local smoke/perf suite by design
  (depends on live accounts and a running server).

## Non-goals

- No mock/hermetic provider; no CI integration.
- No eviction policy work (memory stays bounded by mailbox count × depth).
- No change to list warming (already at 150) or split-count warming.

## Error handling

- Warmer failures stay per-mailbox non-fatal (existing `warn!` pattern);
  the cache-skip filter treats a poisoned/missing cache entry as "not
  cached" and fetches.
- Playwright: accounts that go pending mid-run are skipped with an
  annotation; thresholds are per-assertion so one slow account fails
  loudly with its account id in the test name.

## Success criteria

- After a warm cycle, opening any of the top 150 inbox emails on any
  connected account does not hit the provider (API ≤ 300 ms) and renders
  ≤ 1 s.
- A freshly authorized account has a warm inbox within ~1 minute (one-shot
  warm) rather than up to 5.
- `cd e2e && npx playwright test` passes against the live server.

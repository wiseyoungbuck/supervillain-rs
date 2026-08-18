# Supervillain: Multi-Agent Implementation Plan for Outstanding Kata Tickets

## Context

20 kata tickets are open in `~/scripture/supervillain` (Rust axum email server + single-file
7k-line `static/app.js` frontend + mobile PWA). Goal: implement all **code** tickets using
4 Claude Code agents running simultaneously in Herdr panes (Herdr 0.8.0 live, HERDR_ENV=1),
sequenced to minimize file collisions and bugs. Every feature: **behavioral red-green TDD**
(failing observable-outcome test committed first), **Carmack/Blow/Muratori style** (straight-line
control flow, no speculative abstraction, compress only on real repetition), and **≥1
performance test** (budget asserts — no perf precedent exists; this establishes it, modeled
on rate_limit.rs's CI-tolerant timing asserts).

User decisions: all code tickets in scope; DMARC ×3 (kg7g/ss98/cnqm) + iPhone test (3fh5)
are human follow-ups; 3-4 agents peak; perf = budget asserts.

## Inventory findings

**Already done — verify then close, don't implement:**
- **x7df** (extract api.js): DONE in code. static/api.js exists, desktop+mobile use makeApi, mobile/jmap.js deleted. Verify checklist, close.
- **8e3w**: bind is loopback-default with SUPERVILLAIN_BIND override; SW versioned cache done. Remainder: `tailscale serve` runbook/docs + live verify.
- **zqrn**: account-level signatures DONE. Remainder: per-identity (swap signature when compose-From changes) — subject to the desktop/mobile byte-pin.

**To build:** map4, vj6k+acag (combined — see below), pakx, e993, e2h4, wcsg, j6e4, mtqp, 6rhw, 5np4, ngzw. (1v8z is an epic that closes via its children.)

**Design decision — combine vj6k (Undo Send) + acag (Send Later):** one deferred-send
queue (new `src/scheduled_send.rs` modeled on reminders.rs ReminderStore + 30s tokio daemon,
spawned in main.rs). Undo Send = Send Later with a short default delay + cancel endpoint.
One doSendEmail rewrite, one queue, two kata closes. Building separately = rewriting the
same 195 pinned lines twice.

## Track & wave schedule (4 Herdr agents, 4 worktrees)

| Track | Worktree | Exclusive ownership |
|---|---|---|
| **A: Send path** | `../sv-a` | doSendEmail (app.js 2284-2477), types.rs EmailSubmission, provider.rs send arms, new scheduled_send.rs, tracking pixel |
| **B: List/keyboard** | `../sv-b` | palette switches, renderEmailList (2737-2802), handleNormalModeKey (4102-4271), selection model, move picker |
| **C: Detail/accounts** | `../sv-c` | renderEmailDetail (2804-2852) + detail markup, accounts.rs, oauth.rs, composeSignaturePrefill |
| **D: Calendar/infra** | `../sv-d` | calendar.rs, calendar UI, docs; **merge captain** in later waves |

| Wave | Track A | Track B | Track C | Track D |
|---|---|---|---|---|
| **0** | x7df verify+close (warmup); then vj6k+acag **backend only**: queue store, send_at on EmailSubmission, 3 provider arms, daemon + Rust tests. Zero app.js. | **map4 palette** — sole app.js owner this wave. Merges at wave end; all rebase. | **ngzw** Fastmail OAuth calendar (accounts.rs/oauth.rs, no app.js) | **j6e4 backend** (multi-event CalDAV range query in calendar.rs — doesn't exist yet — + Rust tests); **8e3w** runbook/docs |
| **1** | vj6k+acag **frontend**: doSendEmail rewrite, undo toast, Send Later picker (reuse remind-picker pattern app.js 1234-1323), .cjs + e2e | **e993** move picker → **pakx** bulk selection | **wcsg** contact sidebar → **6rhw** AI assist | **j6e4 frontend** peek view → **mtqp** availability |
| **2** | **e2h4** tracking pixel | **5np4** triage mode | **zqrn** per-identity signatures | Merge captain; spillover (e2h4 or 5np4 if A/B drag — ownership transfers, never overlaps) |

Serialization edges honored: map4 before all palette appends; vj6k+acag→e2h4 (send path);
e993→pakx→5np4 (list/keyboard); wcsg→6rhw (detail pane); j6e4→mtqp (CalDAV).
Critical path: map4 → pakx → 5np4; keep map4 tightly scoped.

## Integration strategy

- **Worktrees**: one long-lived worktree + branch per track (`track/a`…`track/d`), created via `herdr worktree` or `git worktree add`. One commit series per ticket.
- **Merge cadence**: merge to `main` per completed ticket; fixed contention order **A→B→C→D** when simultaneous. Hard sync at end of wave 0 (map4 lands; everyone rebases before touching app.js).
- **Gates**: merging agent rebases onto latest main, then runs `cargo fmt --check`, `cargo clippy -- -D warnings`, full `cargo test` (drives node .cjs + Playwright) before merge. Track D re-runs `cargo test` on main after each landing.
- **TDD in history** (every ticket): `test(<ref>): red — <observable behavior>` commit (agent verifies it actually fails) → `feat(<ref>): green` → optional `refactor(<ref>)`. kata close only after merge + green main.
- **Seam discipline** (in every prompt): shared seams = state literal (app.js 4-89), els bindings (188-443), palette switches, handleKeyDown early-returns (3825-3880), routes.rs router chain (145-205), index.html, style.css. Rules: (1) append-only at seam blocks; (2) one `seam:` commit containing only seam lines; (3) never insert between `commandsForView` and `getCommands` (palette_test.cjs extracts them as one contiguous region); (4) edits to the 7 byte-pinned desktop/mobile functions (routes.rs:12417) touch both files in one commit and run the pin test.

## Per-ticket test targets (behavioral + perf budget)

| Ticket | Behavioral test | Perf test (CI-tolerant budgets, ~5x local, per rate_limit.rs precedent) |
|---|---|---|
| map4 | Extend palette_test.cjs (command → observable state change) + e2e spec | Filter 1,000 synthetic commands < 100ms (.cjs) |
| e993 | New move_picker_test pair: picker innerHTML + POST /move contract; e2e keyboard flow | Render picker over 500 mailboxes < 100ms |
| pakx | bulk_ops_test pair: selection state after keystrokes, batch request contract; e2e | renderEmailList 1,000 emails / 500 selected < 150ms |
| 5np4 | Advance-after-action semantics in .cjs; e2e triage run | 100 triage keystrokes over 1,000-email list < 200ms total |
| wcsg | contact_sidebar_test pair: detail render includes sidebar given mocked contacts | Sidebar render, 200-message contact < 50ms |
| 6rhw | Rust endpoint contract test w/ mock upstream + .cjs rendered state | Prompt assembly (no network) < 50ms (Instant) |
| j6e4 | Rust: CalDAV range query parse from fixture ICS; .cjs peek render | Parse 1,000-event range < 100ms (Instant) |
| mtqp | Rust: busy-blocks → free-slots pure-function test | 5,000 events × 4-week window < 50ms |
| ngzw | Rust: OAuth exchange + refresh contract vs mock server | Token-refresh scheduling over 50 accounts < 10ms |
| vj6k+acag | Rust: queue schedule→due→dispatch + cancel-before-due; scheduling-semantics asserts; .cjs: doSendEmail defers, send_at in body, cancel restores draft; e2e undo toast | Daemon tick scan over 10,000 queued < 50ms |
| e2h4 | Rust: pixel injected (unique, idempotent); open handler returns 1×1 gif + records | Injection into 1MB HTML < 20ms |
| zqrn | .cjs: From change swaps signature (observable compose body); byte-pin test green | Prefill 50 identities × 10KB sigs < 20ms |
| x7df/8e3w | Verify existing suites green; runbook doc | n/a (verify/doc tickets) |

## Top risks baked into agent prompts

1. **doSendEmail's pinned invariants** (roborev-pinned autosave-gate line, session guards): Track A writes a green *characterization* .cjs test of current behavior BEFORE the red-green cycle; sole owner of 2284-2477.
2. **Desktop/mobile byte-pin** (app.js:4675 ↔ mobile/app.js:1990): mirror in one commit, run pin test immediately.
3. **palette string-pin**: preserve commandsForView/getCommands adjacency; run palette_test after any palette touch.
4. **app.js merge conflicts**: region-ownership map in every prompt ("you own X; touching Y is a bug"), append-only seam commits, fixed rebase order.
5. **Flaky perf budgets**: generous budgets + tolerance comments; prefer semantics asserts over tight wall-clock; synthetic data, never network. Also: grep tests/*.cjs for a function name before renaming anything (extractFunction string pins).

## Herdr orchestration (execution mechanics)

1. This session (orchestrator, in a Herdr pane) creates 4 worktrees, then per track:
   `herdr pane split --current --direction right|down --no-focus` → read pane_id from JSON →
   `herdr pane rename <id> "track-a-sendpath"` → `herdr pane run <id> "claude"` (interactive) →
   `herdr wait agent-status <id> --status idle --timeout 30000` → `herdr pane run <id> "<track prompt>"`.
2. Track prompts contain: ticket refs + `kata show` refs, exclusive-region ownership list,
   seam rules, TDD commit protocol, perf-test spec, merge-gate checklist, and "claim your
   tickets with `kata claim`, comment progress, close on green main".
3. Orchestrator monitors: `herdr wait agent-status <id> --status done` (background tabs) or
   `--status blocked` → `herdr pane read <id> --source recent-unwrapped --lines 120`; answers
   blocked agents via `herdr pane run`.
4. Wave transitions: orchestrator confirms main is green (`cargo test`), tells each pane to
   rebase, then issues the next ticket prompt to each track.

## Verification

- Per ticket: red commit demonstrably fails → green → `cargo fmt --check && cargo clippy -- -D warnings && cargo test` on the rebased branch → merge → Track D re-verifies main.
- Per wave: full suite green on main; kata tickets closed with a comment linking commits.
- End state: `kata list --status open` shows only the human tickets (DMARC ×3, 3fh5) and 1v8z if children remain; final `/run` smoke of the app (compose→send-later→cancel; bulk archive; palette commands; calendar peek).
- Human follow-ups to hand back: DMARC DNS changes (kg7g/ss98/cnqm), iPhone PWA test (3fh5, after 8e3w's tailscale serve is clicked).

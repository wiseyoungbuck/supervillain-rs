# P0 close-out plan — 8e3w / x7df / hp8w / yane

> **STATUS: COMPLETED — historical document (roborev 367/370).** The
> `hp8w`/`yane` fixes shipped in `c8e126a`/`b305f2b` (merged at `dda2633`,
> 2026-07-27); the `ceph`/`fpjj` work referenced as "dirty working tree"
> (including §6's stash instructions) has long since landed. `8e3w`/`x7df`
> remain operational-only (tailnet serve + device verification, tracked in
> kata). Nothing below is a live instruction; ground-truth claims and line
> numbers reflect `main` @ `3c617f0` and have drifted.

A design plan in the spirit of Carmack, Muratori, and Blow: find the root
cause, make the smallest change that is actually correct, delete rather than
add, write the test that pins the real behavior, and be honest about what is
already done so we don't invent work.

Status as verified on `main` @ `3c617f0`. Note: `3c617f0` *is* the `ceph`
merge (iframe partial-render fix); the dirty working tree is `ceph` **follow-up**
plus its `tests/email_iframe_test.{cjs,rs}` files (uncommitted, see §6), and
`fpjj` mobile split-counts — none of which is on `main`. Line numbers below are
from the dirty tree and drift ~30 lines ahead of `main`; the tests anchor by
**function name** (via `js_fn_body`), not line, so the drift is cosmetic — but
if you jump to a cited line on `main`, expect the offset.

_Revised after a `roborev review --type design` pass (claude-code): the plan
now (a) states Tier 2 is the first committed behavioral instance rather than
mirroring a harness that isn't on `main`, (b) inserts the missing deploy step,
(c) corrects the `x7df` commit attribution, (d) exercises `escapeAttr`'s
single-quote branch, and (e) adds a manual repro-then-verify step and a stale-
comment fix._

---

## 0. Ground truth — what is actually broken

| Ticket | Kind | Code state on `main` | Remaining work |
|--------|------|----------------------|----------------|
| **8e3w** | infra/mobile | **Code-complete.** Bind is loopback-default (`src/main.rs:152` `bind_addr`), `SUPERVILLAIN_BIND` env override, launcher+upgrade default+validate `127.0.0.1:8000` with `scripts/tests/test_launcher_bind.sh`. SW version-busts via `__SUPERVILLAIN_VERSION__` substituted in the `mobile_sw` route handler (`static/mobile/sw.js:5`, never served from disk); `APP_SHELL` carries the `/mobile/index.html` alias (`sw.js:10`); `activate` purges old caches; never caches `/api/`. SW registration guarded by `window.isSecureContext` (`static/mobile/app.js:3179`). **README runbook is in** (`README.md:359-388`: `tailscale serve --bg --https=443`, Caddy fallback, "Nothing assumes Tailscale", breaking-change note). | **Operational, not code.** One manual `tailscale serve` command (after the one-time admin-console approval the CLI already printed a link for) + on-device verification (tracked in `3fh5`). |
| **x7df** | feature/mobile | **Code-complete.** `static/api.js` exists: `makeApi` (`:38`), `ApiError`/`ApiAuthError` taxonomy, `ACCOUNT_SCOPED_API` auto-append of `?account=`. Desktop rewired (`api()`/`apiWithMeta` wrap `makeApi`). Mobile rewired (loads `api.js`, `makeApi(null)`, account switcher). `static/mobile/jmap.js` deleted (not in tree), Fastmail-token login + `STORAGE_KEY` session path gone. Append-only pagination shipped. Net `-426` LOC landed at `93433c8` (the
`api.js` extraction + rewire); `af815a2` is a separate 4-line unread-contrast
fix *found* during x7df verification, not part of the rewire — don't credit it
to the pivot. | **Verification, not code.** Phone-over-tailnet latency + 15-min device dogfood (Fastmail↔Gmail switch), blocked on `8e3w`'s tailnet enablement. |
| **hp8w** | bug/security | ~~NOT shipped~~ **Shipped** in `c8e126a` (merged `dda2633`, 2026-07-27). | None. |
| **yane** | bug/security | ~~NOT shipped~~ **Shipped** in `b305f2b` (merged `dda2633`, 2026-07-27). | None. |

**Reframing.** "Fix all four" is really: **land 4 one-line fixes (2 tickets) +
their tests, then run 1 command + 1 device pass to close the other 2.** Two of
the four tickets have no design or build left. We say that plainly and do not
pad the plan with make-work.

---

## 1. The one root cause

Both security tickets are the same bug wearing two hats:

> **We build HTML by string interpolation, and the safety of the result depends
> on the author remembering, at each `${...}`, whether the fragment is
> attacker-controlled.** In `hp8w` the fragment is a *text-content* slot (the
> From display name); in `yane` it is an *attribute* slot (the attendee email
> in `title="..."`). Both sinks land in `innerHTML`.

The codebase already has the two correct primitives for this exact split:

- `escapeHtml` (`static/app.js:4892`) — `textContent` round-trip; neutralizes
  `< > &` for **text content**.
- `escapeAttr` (`static/app.js:4901`) — `escapeHtml` + `"`→`&quot;` and
  `'`→`&#39;`; for **attribute values**, where `escapeHtml` alone is insufficient
  because the `textContent` serializer does not encode quotes.

The fix is not to invent a template engine or a sanitizer wrapper. The fix is
to call the primitive that already exists, at the two sites that forgot to.
`startForward` (the sibling one function down) already does it correctly —
`startReply` is the one that didn't. The proportionate response to "two sites
forgot the helper that every other site uses" is "make those two sites use it,"
not "rewrite rendering."

The two collateral bugs in `hp8w` have their own one-line root causes
(`object === string` dead branch; uncanceled timer) — no shared system, just
two small correctness failures riding in the same ticket.

---

## 2. `hp8w` — reply-quote XSS + two collateral desktop bugs

All three live in **desktop `static/app.js` only.** Mobile already escapes its
reply header (`static/mobile/app.js:1922`) and renders an attendee *count*, so
it is unaffected by this ticket. Net LOC: flat.

### 2a. XSS — `startReply` quote header (`static/app.js:3884`)

Today:

```js
const header = `On ${formatDate(email.receivedAt)}, ${from?.name || from?.email} wrote:`;
renderComposeQuote(header, quotedHtml, quotedText);
```

`renderComposeQuote` assigns `headerEl.innerHTML = headerHtml` (`:4920`). A From
display name of `<img src=x onerror=alert(document.cookie)>` executes in the app
origin the moment the user hits Reply/Reply-All — no click, no hover.

`formatDate(email.receivedAt)` is trusted-by-construction (it parses an ISO
timestamp and emits our own formatted string), so the attacker-controlled
fragment is exactly `from?.name || from?.email`. `startForward` (`:3905`)
already escapes the same field. Fix: escape the attacker fragment, matching the
sibling's pattern:

```js
const header = `On ${formatDate(email.receivedAt)}, ${escapeHtml(from?.name || from?.email || '')} wrote:`;
```

That is the whole fix. One token inserted.

### 2b. Collateral — `removeAccountById` dead branch (`static/app.js:4547`)

Today:

```js
if (state.currentAccount === id) {
    state.currentAccount = null;
    state.currentEmail = null;
    state.emails = [];
}
```

`state.currentAccount` is an **object**; `id` is a **string**. `object ===
string` is always `false`. The branch is dead — deleting the in-use account
leaves `state.currentAccount` / `state.currentEmail` / `state.emails` pointing
at a vanished account. (`state.selectedAccountId === id` above it is correct
because that one *is* a string.) Fix:

```js
if (state.currentAccount?.id === id) {
```

### 2c. Collateral — `showStatus` timer race (`static/app.js:3057`)

Today:

```js
function showStatus(message, type = 'info') {
    els.statusMessage.textContent = message;
    els.statusMessage.style.color = type === 'error' ? 'var(--danger)' :
                                    type === 'success' ? 'var(--success)' : 'var(--fg-muted)';
    setTimeout(() => {
        els.statusMessage.textContent = '';
    }, 3000);
}
```

Two calls in quick succession (e.g. `showStatus('Deleted…', 'success')` then a
follow-up `showStatus('Failed…', 'error')`): the first call's 3s timer fires and
blanks the newer, more important message. Classic unbounded-timer last-writer
race. Fix — cancel the prior timer, keep one handle:

```js
let statusTimer = null;   // module scope, alongside the other top-level lets

function showStatus(message, type = 'info') {
    clearTimeout(statusTimer);
    els.statusMessage.textContent = message;
    els.statusMessage.style.color = type === 'error' ? 'var(--danger)' :
                                    type === 'success' ? 'var(--success)' : 'var(--fg-muted)';
    statusTimer = setTimeout(() => {
        els.statusMessage.textContent = '';
    }, 3000);
}
```

A generation counter would also work and is "more correct," but for a 3-second
status line a single cancellable handle is the minimal correct fix. No new
machinery.

### 2d. Collateral — stale `bind_addr` doc comment (`src/main.rs:153-154`)

Not a security bug, but while we're in the security/infra-hygiene commit: the
doc comment above `bind_addr` still says scripts bind `0.0.0.0` ("as
`scripts/upgrade.sh` and the launcher do"), but both now default to
`127.0.0.1:8000` (the `8e3w` change). A stale comment that contradicts the code
is exactly the thing that invites the "well-meaning bloat" this plan warns
against — a reader thinks the loopback default is missing and re-adds one.
One-line comment correction, folded into the `hp8w` commit; fits the
delete-don't-add ethos, no behavior change.

---

## 3. `yane` — calendar attendee `title` attribute XSS (`renderCalendarCard` in `static/app.js`)

Today, inside `renderCalendarCard`:

```js
return `<span class="attendee" title="${a.email}">${statusIcon} ${escapeHtml(name)}</span>`;
```

The text content (`name`) is correctly escaped; the **attribute** (`a.email`) is
not. An attendee email of `" onmouseover="alert(1)` (or `"><img src=x
onerror=…>`) breaks out of `title="..."` and runs script on render — no hover
required, because the breakout happens at parse time. `escapeHtml` alone is
insufficient here (it doesn't encode `"`); this is exactly what `escapeAttr`
exists for. Fix:

```js
return `<span class="attendee" title="${escapeAttr(a.email)}">${statusIcon} ${escapeHtml(name)}</span>`;
```

**`statusIcon` is deliberately left unescaped.** It comes from `getStatusIcon`
(`:5562`), which returns one of four fixed `<span>` constants we control
(`&#10003;`, `&#10007;`, `?`, `&#8226;`). Escaping it would double-encode the
entities and break the icons. The invariant to protect — and to write down in a
one-line comment at the call site so the next person doesn't "fix" it — is:
*`statusIcon` is trusted-by-construction; only `a.email` and `name` are
attacker-controlled.* (Comment, not code; the test below pins `escapeAttr(a.email)`
regardless of the comment.)

---

## 4. Test strategy — two tiers, matching the repo's existing harness

The committed tier on `main` is the **code-shape** contract tests in
`src/routes.rs` (~280, via `js_fn_body`). A **behavioral** node-test tier
(`tests/*.cjs` wired through `tests/*.rs`) exists only in the uncommitted
`ceph` follow-up — it is *not* on `main`, so this plan cannot "copy" it.
Instead §4 specifies the first committed behavioral instance self-contained.
We extend the committed tier and add the second tier as a first commit; we do
not pretend a harness that isn't on `main` is there to mirror.

### Tier 1 — code-shape contract tests (`src/routes.rs`)

`js_fn_body(src, "function X(")` slices a function to its column-0 closing
brace; tests then `assert!(block.contains("…"))`. ~280 of these exist. They are
the durable guard against **reintroduction**: they assert on the literal code
form inside the exact function slice, so an explanatory comment cannot satisfy
them (the `ceph`/`kph2` tests say this explicitly). Add, near the existing
escape tests:

```rust
#[test]
fn app_js_start_reply_escapes_sender_in_quote_header() {
    // hp8w: startReply builds the "On <date>, <name> wrote:" header and hands
    // it to renderComposeQuote, which assigns it to innerHTML. The From name
    // is attacker-controlled. startForward already escapes; startReply must
    // too. Match code forms, not prose (roborev 336 #2).
    let block = js_fn_body(APP_JS, "function startReply(");
    assert!(
        block.contains("escapeHtml(from?.name"),
        "startReply must escapeHtml the From name/email in the quote header — \
         renderComposeQuote assigns it to innerHTML"
    );
}

#[test]
fn app_js_calendar_attendee_title_escapes_email() {
    // yane: renderCalendarCard interpolates a.email into title="…". escapeHtml
    // is insufficient in an attribute (no quote encoding), so the call site
    // must use escapeAttr.
    let block = js_fn_body(APP_JS, "function renderCalendarCard(");
    assert!(
        block.contains("escapeAttr(a.email)"),
        "renderCalendarCard must escapeAttr the attendee email in the title \
         attribute — a crafted email breaks out of an unescaped attribute"
    );
}

#[test]
fn app_js_remove_account_compares_id_not_object() {
    // hp8w collateral: state.currentAccount is an object; comparing it to an
    // id string is always false, so the reset branch was dead.
    let block = js_fn_body(APP_JS, "function removeAccountById(");
    assert!(
        block.contains("state.currentAccount?.id === id"),
        "removeAccountById must compare state.currentAccount?.id to the id, \
         not the account object (dead-branch bug)"
    );
}

#[test]
fn app_js_show_status_cancels_prior_timer() {
    // hp8w collateral: an older call's 3s timer must not blank a newer
    // message. The fix cancels the prior timer and keeps one handle.
    // The whole-file contains below is a precondition only — a module-level
    // let can't live inside a function slice, so js_fn_body can't pin it. The
    // load-bearing assertions are the two sliced ones (clearTimeout +
    // setTimeout reassign) inside showStatus itself; those are what a comment
    // cannot satisfy.
    assert!(
        APP_JS.contains("let statusTimer = null;"),
        "showStatus needs a module-level timer handle"
    );
    let block = js_fn_body(APP_JS, "function showStatus(");
    assert!(
        block.contains("clearTimeout(statusTimer)")
            && block.contains("statusTimer = setTimeout"),
        "showStatus must clearTimeout the prior timer and reassign the handle"
    );
}
```

These run on every `cargo test`, no node required, and fail the instant someone
reverts a fix.

### Tier 2 — behavioral test of the escape primitives (`tests/escape_test.cjs` + `tests/escape_test.rs`)

The Tier-1 tests prove the call **sites** use the helpers. They do not prove the
**helpers** actually neutralize the attack. `escapeHtml`/`escapeAttr` touch
`document`, so we prove them behaviorally with the same mock-DOM pattern
`tests/email_iframe_test.cjs` already uses: extract the real functions via
`new Function(...)`, inject a minimal `document.createElement` shim that
implements the `textContent`→`innerHTML` HTML-escaping contract, and assert
real payloads are neutralized.

`tests/escape_test.cjs` (sketch — mirrors `email_iframe_test.cjs`'s extraction
+ `new Function` + injected-globals pattern):

```js
// Behavioral tests for the escape primitives (hp8w / yane).
// Tier-1 (src/routes.rs) pins that the call SITES use escapeHtml/escapeAttr;
// these prove the primitives themselves neutralize the attack classes.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const APP_JS = fs.readFileSync(path.join(__dirname, '..', 'static', 'app.js'), 'utf8');

// Minimal DOM shim: textContent assignment serializes via HTML entity encoding
// for < > & (the browser's textContent→innerHTML contract). innerHTML read
// returns that serialized string. This is the WHOLE behavior escapeHtml relies
// on; we implement exactly that and nothing more.
function makeDocument() {
    return {
        createElement() {
            let _text = '';
            return {
                set textContent(v) { _text = String(v); },
                get innerHTML() {
                    return _text
                        .replace(/&/g, '&amp;')
                        .replace(/</g, '&lt;')
                        .replace(/>/g, '&gt;');
                },
            };
        },
    };
}

function loadEscapeFns() {
    const htmlStart = APP_JS.indexOf('function escapeHtml(');
    const htmlClose = APP_JS.indexOf('\n}', htmlStart);
    const attrStart = APP_JS.indexOf('function escapeAttr(');
    const attrClose = APP_JS.indexOf('\n}', attrStart);
    // escapeAttr calls escapeHtml, so eval both together.
    const code = APP_JS.slice(htmlStart, htmlClose + 2) + '\n'
               + APP_JS.slice(attrStart, attrClose + 2)
               + '\nreturn { escapeHtml, escapeAttr };';
    // eslint-disable-next-line no-new-func
    return new Function('document', code)(makeDocument());
}

test('escapeHtml neutralizes a script-tag payload (hp8w class)', () => {
    const { escapeHtml } = loadEscapeFns();
    const out = escapeHtml('<img src=x onerror=alert(1)>');
    assert.equal(out, '&lt;img src=x onerror=alert(1)&gt;');
    assert.ok(!out.includes('<img'));   // no live tag survives
});

test('escapeAttr neutralizes an attribute-breakout payload (yane class)', () => {
    const { escapeAttr } = loadEscapeFns();
    // Double-quote breakout is the yane vector (" onmouseover="alert(1)). But
    // an attribute can also be SINGLE-quoted, and escapeAttr's .replace(/'/g,
    // '&#39;') is exactly the branch a regression could silently drop — so the
    // payload carries BOTH quote kinds and both breakouts are asserted away.
    const out = escapeAttr('" onmouseover="alert(1) \' onmouseover=\'alert(2)');
    assert.ok(out.includes('&quot;'), 'double quotes must be encoded');
    assert.ok(out.includes('&#39;'), 'single quotes must be encoded');
    assert.ok(!out.includes('"onmouseover"'), 'no double-quote breakout pair survives');
    assert.ok(!out.includes("'onmouseover'"), 'no single-quote breakout pair survives');
    // no raw quotes of either kind survive
    assert.equal((out.match(/["']/g) || []).length, 0);
});
```

Wire it into `cargo test` with a **self-contained** `tests/escape_test.rs`.
The `email_iframe_test.rs` it resembles is uncommitted `ceph` work, *not* on
`main` — do not depend on it; the `.cjs` sketch above is already complete on
its own. Spec the wrapper fully rather than "copy":

```rust
//! Behavioral test for the escape primitives (kata hp8w / yane). Shells out
//! to `node --test`, and skips (does not fail) if node is absent, so CI
//! images without node stay green. The string-invariant Tier-1 tests in
//! src/routes.rs guard the fix regardless of node availability.
use std::path::PathBuf;
use std::process::Command;

#[test]
fn escape_primitive_behavior_tests_pass() {
    let dir = env!("CARGO_MANIFEST_DIR");
    let test_js: PathBuf = [dir, "tests", "escape_test.cjs"].iter().collect();
    assert!(test_js.exists(), "tests/escape_test.cjs must exist");
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("node not on PATH; skipping escape behavior tests");
        return;
    }
    let out = Command::new("node").arg("--test").arg(&test_js)
        .output().unwrap_or_else(|e| panic!("failed to spawn node: {e}"));
    assert!(
        out.status.success(),
        "escape behavior tests failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
```

**Why this layering and not just one tier.** The code-shape tests are fast and
reintroduction-proof but prove nothing about *correctness* — they'd pass against
an `escapeHtml` that did nothing. The behavioral test proves the primitives
work but says nothing about *which call sites use them*. Together: the fix is
present at the site **and** the fix actually neutralizes the attack. Neither can
be passed by a comment. That is the whole point.

### Manual confirmation (cheap, closes the loop on the real attack)

Neither tier renders the composed header in a real DOM — Tier 1 is code-shape,
Tier 2 tests the primitives in isolation. So before closing `hp8w`/`yane`, one
manual repro-then-verify pass on the **deployed** binary: send yourself a test
email with a From display name of `<img src=x onerror=alert(1)>` (hp8w) and a
calendar invite with an attendee email of `" onmouseover="alert(1)` (yane),
then Reply / open the invite and confirm no `alert` fires and no stray element
appears. This is the end-to-end proof the unit tests structurally can't give,
and it costs minutes. Do it after the §6 deploy step, against the running
server — not against a pre-deploy binary.

---

## 5. Closing `8e3w` and `x7df` — by running, not building

There is nothing to design or build here. The code is on `main`. What remains is
the operational gate the tickets were deliberately held open for.

### `8e3w` (~5 minutes at the machine)

1. If not yet done: click the Tailscale admin-console enablement link the CLI
   printed (one-time, per-tailnet).
2. `tailscale serve --bg --https=443 http://127.0.0.1:8000`
3. `tailscale serve status` to confirm.
4. From the phone, on the tailnet: `https://<host>.<tailnet>.ts.net/mobile/`
   loads.
5. Confirm the SW registered under the secure context (it silently no-ops over
   plain http — `static/mobile/app.js:3179`): Safari → Settings → Website Data
   shows the cache, or `navigator.serviceWorker.getRegistrations()` from an
   inspector.
6. Share → Add to Home Screen → launches standalone.
7. Close `8e3w`. This unblocks `3fh5` (its `blocked-by`).

**Do not** add TLS termination, auth middleware, or a config file to the server.
The tailnet is the auth and transport boundary; the server's job ends at
loopback. (Stated in the ticket; restated here because it is the kind of thing
that invites well-meaning bloat.)

### `x7df` (after `8e3w` is up)

With the tailnet serving HTTPS, run the on-device verification the ticket was
held open for: phone-over-tailnet latency numbers (the comment records ~0.2ms
accounts / ~0.08ms warm list on loopback — confirm the tailnet path is
acceptable), and a 15-minute dogfood exercising the Fastmail↔Gmail account
switcher from the phone. When `3fh5`'s daily-drive script passes, close `x7df`
together with it.

---

## 6. Sequencing & merge hygiene

The working tree is **dirty with unrelated in-flight work** (`ceph` iframe
re-measure in `src/routes.rs`/`static/app.js`/`static/mobile/app.js` and
`tests/email_iframe_test.{cjs,rs}`; `fpjj` mobile split-counts in
`src/routes.rs`/`static/mobile/app.js`). The security fixes must not be
entangled with that.

1. **Isolate.** The working tree is dirty with uncommitted `ceph` follow-up +
   its `tests/email_iframe_test.{cjs,rs}` and `fpjj` split-counts — and both
   touch `src/routes.rs`, the very file the `hp8w` Tier-1 tests must be added
   to. "Leave it dirty" would entangle: `git add src/routes.rs` would stage the
   ceph/fpjj hunks too. So **stash first**: `git stash push -u -m ceph-fpjj`
   (the `-u` includes the untracked test files), then branch `fix/p0-security`
   off a clean `main`. The security commits now touch a clean `routes.rs` and
   add only their own test hunks + `tests/escape_test.{cjs,rs}`. After merge,
   `git stash pop` resumes the `ceph`/`fpjj` work — or land those first and
   skip the stash; either way the security commits never carry ceph/fpjj diff.
2. **Commit `hp8w`** — the 3 one-line fixes (`startReply`, `removeAccountById`,
   `showStatus` + `let statusTimer`) plus the §2d `bind_addr` comment
   correction, and its Tier-1 + Tier-2 tests, in one commit. Subject in the
   repo idiom: `Fix reply-quote XSS + account-removal/status bugs (kata hp8w)`.
3. **Commit `yane`** — the 1 one-line fix (`escapeAttr(a.email)`) and its
   Tier-1 + Tier-2 test. Subject: `Fix calendar attendee title XSS (kata yane)`.
   Separate commit because it is a separate kata id and a separate finding; the
   repo keys commits to kata ids.
4. `cargo test` (Tier-1 always; Tier-2 if node present), `cargo clippy -- -D
   warnings`, `cargo fmt`.
5. Merge to `main` (the repo's flow is `fix/*` → merge commit → `main`).
6. **Deploy.** Merge ≠ serve — static assets are `include_str!`-compiled into
   the binary, so the running server still serves the pre-fix binary until
   rebuilt. Run `./scripts/upgrade.sh` (the documented stop/rebuild/restart
   path) and confirm the new build is up (the `/api/build-id` poller sees the
   new id, or just re-hit the server). Skip this and the §5 on-device pass +
   the §4 manual XSS re-check exercise the *old* code — the tickets would close
   on a fix that isn't running.
7. Then run the §5 runbook to close `8e3w` → `3fh5` → `x7df`.

Two commits, ~8 lines of production code, ~6 lines of JS, ~60 lines of tests.
Net LOC flat-to-negative.

---

## 7. What we deliberately do NOT do (the class-vs-instance call)

A scan of `static/app.js` shows ~50 `innerHTML =` sinks. The two reported XSS
sites are the ones with a **confirmed attacker-controlled unescaped fragment**.
The Carmack/Muratori/Blow reading of this is not "there are 50 sinks, rewrite
all rendering" — that is the bloat reflex, it grows LOC, it risks regressions in
a working app, and it addresses a *speculative* class rather than a *demonstrated*
bug. The proportionate response:

- **Fix the demonstrated instances** (`hp8w`, `yane`) — done in §2-3.
- **Make the safe primitive the obvious default.** It already is: `escapeHtml`
  is used at ~30 sites in `app.js` and throughout `mobile/app.js`; `escapeAttr`
  exists specifically for the attribute case and is used at the data-attribute
  sites. The two bugs are the *exceptions that forgot*, not a systemic
  wrong-default. The tests in §4 make "forgot" a build failure going forward.
- **Do not** add a tagged-template sanitizer (`html\`…\``), a virtual-DOM
  layer, or a "SanitizedRender" wrapper. Each would be new machinery the
  program now depends on, none of which the two bugs require, and each would
  shift the trust boundary from "two audited functions" to "a new abstraction
  that must itself be audited." The existing iframe sandbox
  (`renderHtmlBodyIframe`, sandbox with **no** `allow-scripts`) already closes
  the *inbound* HTML class; `escapeHtml`/`escapeAttr` close the *our-UI-strings*
  class. The reply-quote header was the one place our UI composed attacker text
  into HTML — that is precisely why it was the bug. Fixing it makes it
  consistent with the rest, not the start of a refactor.
- **Do not** expand scope to the other ~48 sinks in this change. If a future
  audit finds another confirmed unescaped attacker fragment, it gets its own
  ticket, its own one-line fix, and its own Tier-1/Tier-2 test — the same
  discipline, not a flag-day rewrite.

The durable defense is already in the codebase: the no-`allow-scripts` iframe
sandbox for inbound mail HTML, and `escapeHtml`/`escapeAttr` for our own UI
strings. This plan makes the two sites that violated that defense comply, pins
them with tests that can't be satisfied by prose, and closes the two tickets
that were only ever waiting on a command and a phone tap.

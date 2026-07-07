# Account-Scoped Splits Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Tag each split inbox with an owning account so split tabs only render on — and "primary" is only computed against — the account they belong to.

**Architecture:** `SplitInbox` gains an optional `account` field (config-section id; `None` = all accounts). A new `SplitsConfig::scoped_to(Option<&str>)` filters the loaded config; the four consumers (`list_splits`, `list_emails`, `split_counts`, prefetch warmer) scope before filtering/counting. Seeding tags generated splits. Frontend sends `?account=` on splits reads and tags new splits with the active account.

**Tech Stack:** Rust (edition 2024), axum, serde; vanilla JS frontend in `static/app.js`.

**Spec:** `docs/superpowers/specs/2026-07-06-account-scoped-splits-design.md`

## Global Constraints

- `cargo test` green, `cargo clippy -- -D warnings` clean, `cargo fmt` applied before every commit.
- No new files; all changes land in existing modules.
- Existing splits.json files without `account` fields must keep parsing and behaving as today (untagged = visible everywhere).
- Split ids remain globally unique across accounts (existing duplicate-id check unchanged).
- Line numbers below are from the plan-writing snapshot; locate by the quoted code, not the number.

---

### Task 1: `account` field on `SplitInbox` + serde back-compat

**Files:**
- Modify: `src/types.rs` (struct at ~line 340; tests at ~673, ~686, ~810, ~828)
- Modify: `src/splits.rs` (test literals; production literal in `generate_splits_from_identities` at ~line 130)

**Interfaces:**
- Produces: `SplitInbox.account: Option<String>` — serde name `account`, absent when `None`. Every later task relies on this exact field name.

- [ ] **Step 1: Write the failing tests** — in `src/types.rs`, add to the tests module directly after `split_inbox_icon_present_in_json`:

```rust
    #[test]
    fn split_inbox_account_roundtrip() {
        let split = SplitInbox {
            id: "work".into(),
            name: "Work".into(),
            icon: None,
            filters: vec![],
            match_mode: MatchMode::Any,
            account: Some("aristoi".into()),
        };
        let json = serde_json::to_string(&split).unwrap();
        assert!(json.contains(r#""account":"aristoi""#));
        let back: SplitInbox = serde_json::from_str(&json).unwrap();
        assert_eq!(back.account.as_deref(), Some("aristoi"));
    }

    #[test]
    fn split_inbox_account_absent_parses_as_none() {
        // Back-compat: every pre-existing splits.json lacks the field.
        let json = r#"{"id": "x", "name": "X", "filters": [], "match_mode": "any"}"#;
        let split: SplitInbox = serde_json::from_str(json).unwrap();
        assert_eq!(split.account, None);
    }

    #[test]
    fn split_inbox_account_none_omitted_from_json() {
        let split = SplitInbox {
            id: "test".into(),
            name: "Test".into(),
            icon: None,
            filters: vec![],
            match_mode: MatchMode::Any,
            account: None,
        };
        let json = serde_json::to_string(&split).unwrap();
        assert!(!json.contains("account"));
    }
```

- [ ] **Step 2: Run to verify RED**

Run: `cargo test split_inbox_account 2>&1 | tail -20`
Expected: compile error `E0560: struct SplitInbox has no field named account`.

- [ ] **Step 3: Add the field** — in `src/types.rs`, change the struct to:

```rust
pub struct SplitInbox {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default)]
    pub filters: Vec<SplitFilter>,
    #[serde(default)]
    pub match_mode: MatchMode,
    /// Config-section account id this split belongs to.
    /// `None` = visible on every account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}
```

- [ ] **Step 4: Fix every struct literal** — add `account: None,` after the `match_mode:` line in **all** existing `SplitInbox {` literals. Snapshot locations — `src/splits.rs`: 130 (inside `generate_splits_from_identities`), 446, 462, 484, 504, 523, 634, 652, 680, 687, 829, 882, 897, 914; `src/types.rs`: 673, 686, 810, 828 (the three new tests from Step 1 already carry the field). Do NOT rely on this list — run `cargo test --no-run 2>&1 | grep E0063` until zero missing-field errors remain; the compiler is the source of truth.

- [ ] **Step 5: Run to verify GREEN**

Run: `cargo test 2>&1 | tail -5`
Expected: all tests pass, 0 failed.

- [ ] **Step 6: Lint, format, commit**

```bash
cargo clippy -- -D warnings && cargo fmt
git add src/types.rs src/splits.rs
git commit -m "Add optional account field to SplitInbox"
```

---

### Task 2: `SplitsConfig::scoped_to` + primary-semantics test + module doc

**Files:**
- Modify: `src/splits.rs` (new impl after `save_splits`; tests; module doc at lines 1–14)

**Interfaces:**
- Consumes: `SplitInbox.account` (Task 1).
- Produces: `pub fn scoped_to(&self, account: Option<&str>) -> SplitsConfig` on `SplitsConfig`. Task 4 calls exactly this.

- [ ] **Step 1: Write the failing tests** — in `src/splits.rs` tests module, after the `to_filter` helper add a builder, and after the `filter_by_split` tests add:

```rust
    fn tagged_split(id: &str, pattern: &str, account: Option<&str>) -> SplitInbox {
        SplitInbox {
            id: id.into(),
            name: id.into(),
            icon: None,
            filters: vec![to_filter(pattern)],
            match_mode: MatchMode::Any,
            account: account.map(String::from),
        }
    }

    // --- scoped_to ---

    #[test]
    fn scoped_to_keeps_own_and_untagged_drops_others() {
        let config = SplitsConfig {
            splits: vec![
                tagged_split("aristoi", "*@aristoi.ai", Some("aristoi")),
                tagged_split("gmail", "*@gmail.com", Some("gmail")),
                tagged_split("calendar", "*@cal.test", None),
            ],
        };
        let ids: Vec<String> = config
            .scoped_to(Some("aristoi"))
            .splits
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids, ["aristoi", "calendar"]);
    }

    #[test]
    fn scoped_to_none_returns_all() {
        let config = SplitsConfig {
            splits: vec![
                tagged_split("aristoi", "*@aristoi.ai", Some("aristoi")),
                tagged_split("calendar", "*@cal.test", None),
            ],
        };
        assert_eq!(config.scoped_to(None).splits.len(), 2);
    }

    #[test]
    fn scoped_to_unknown_account_keeps_only_untagged() {
        // A split tagged to a since-deleted account is never listed.
        let config = SplitsConfig {
            splits: vec![
                tagged_split("old", "*@old.test", Some("deleted-account")),
                tagged_split("calendar", "*@cal.test", None),
            ],
        };
        let scoped = config.scoped_to(Some("gmail"));
        assert_eq!(scoped.splits.len(), 1);
        assert_eq!(scoped.splits[0].id, "calendar");
    }

    #[test]
    fn primary_with_scoped_config_ignores_other_accounts_splits() {
        // The bug this feature fixes: a *@gmail.com split visible on every
        // account swallowed all mail on the gmail account, emptying Primary
        // — and conversely gmail-bound mail vanished from other accounts'
        // Primary. Scoped away, the split must not claim the mail.
        let emails = vec![make_email_with_to("alice@x.com", "matt@gmail.com", &[])];
        let config = SplitsConfig {
            splits: vec![tagged_split("gmail", "*@gmail.com", Some("gmail"))],
        };
        let scoped = config.scoped_to(Some("aristoi"));
        let primary = filter_by_split(emails, "primary", &scoped);
        assert_eq!(primary.len(), 1);
    }
```

- [ ] **Step 2: Run to verify RED**

Run: `cargo test scoped_to 2>&1 | tail -10`
Expected: compile error `E0599: no method named scoped_to found for struct SplitsConfig`.

- [ ] **Step 3: Implement** — in `src/splits.rs`, after `save_splits`:

```rust
// =============================================================================
// Account scoping
// =============================================================================

impl SplitsConfig {
    /// Splits visible to `account`: untagged splits (visible everywhere)
    /// plus splits tagged with exactly this account. `None` returns the
    /// full config — the management view.
    pub fn scoped_to(&self, account: Option<&str>) -> SplitsConfig {
        let Some(account) = account else {
            return self.clone();
        };
        SplitsConfig {
            splits: self
                .splits
                .iter()
                .filter(|s| s.account.as_deref().is_none_or(|a| a == account))
                .cloned()
                .collect(),
        }
    }
}
```

- [ ] **Step 4: Rewrite the module doc** — replace `src/splits.rs` lines 1–14 (the `//!` block asserting splits cannot be account-scoped) with:

```rust
//! Split inbox filters.
//!
//! `splits.json` is a single file, but each `SplitInbox` may carry an
//! `account` tag (a config-section id, e.g. "aristoi"). Tagged splits
//! exist only for that account; untagged splits apply to every account.
//! Route handlers scope the loaded config with [`SplitsConfig::scoped_to`]
//! before filtering or counting, so the synthetic "primary" split means
//! "not matching any of *this account's* splits". A split tagged to a
//! since-deleted account is never listed but stays in the file for
//! hand-editing.
//!
//! Filters run against parsed `Email` objects after fetch, so the same
//! definition works identically on Fastmail, Outlook, and Gmail.
//!
//! Auto-seeding (see [`seed_from_identities`]) runs ONCE at startup
//! against the **default account's** identities, only when `splits.json`
//! is empty, and tags every generated split with that account. It
//! deliberately does not re-run when accounts are added later: doing so
//! would silently clobber the user's edits.
```

- [ ] **Step 5: Run to verify GREEN**

Run: `cargo test 2>&1 | tail -5`
Expected: all tests pass, 0 failed.

- [ ] **Step 6: Lint, format, commit**

```bash
cargo clippy -- -D warnings && cargo fmt
git add src/splits.rs
git commit -m "Add SplitsConfig::scoped_to for per-account split visibility"
```

---

### Task 3: Seeding tags the generating account

**Files:**
- Modify: `src/splits.rs` (`seed_from_identities`, `generate_splits_from_identities`, their tests)
- Modify: `src/main.rs` (call site at ~line 79)

**Interfaces:**
- Consumes: `SplitInbox.account` (Task 1).
- Produces: `generate_splits_from_identities(identities: &[Identity], account: &str) -> SplitsConfig` and `seed_from_identities(identities: &[Identity], account: &str, config_path: &Path) -> Option<SplitsConfig>` — note the new middle parameter.

- [ ] **Step 1: Write the failing test** — in `src/splits.rs` tests, after `generate_splits_case_insensitive_domains`:

```rust
    #[test]
    fn generated_splits_are_tagged_with_seeding_account() {
        let identities = vec![
            make_identity("user@aristoi.ai"),
            make_identity("user@gmail.com"),
        ];
        let config = generate_splits_from_identities(&identities, "aristoi");
        assert_eq!(config.splits.len(), 2);
        assert!(
            config
                .splits
                .iter()
                .all(|s| s.account.as_deref() == Some("aristoi"))
        );
    }
```

- [ ] **Step 2: Run to verify RED**

Run: `cargo test generated_splits_are_tagged 2>&1 | tail -10`
Expected: compile error `E0061: this function takes 1 argument but 2 arguments were supplied`.

- [ ] **Step 3: Implement** — change both signatures and the constructor:

```rust
pub fn seed_from_identities(
    identities: &[crate::types::Identity],
    account: &str,
    config_path: &Path,
) -> Option<SplitsConfig> {
```
(and inside it: `let config = generate_splits_from_identities(identities, account);`)

```rust
pub fn generate_splits_from_identities(
    identities: &[crate::types::Identity],
    account: &str,
) -> SplitsConfig {
```
(and in its `SplitInbox` literal: `account: Some(account.to_string()),` replacing `account: None,`)

In `src/main.rs`, the call becomes:

```rust
                if let Some(config) =
                    splits::seed_from_identities(&identities, &default_account, &splits_config_path)
```

Update every existing test call site — all `generate_splits_from_identities(&identities)` → `generate_splits_from_identities(&identities, "acct")` and all `seed_from_identities(&identities, &path)` → `seed_from_identities(&identities, "acct", &path)`. Affected tests (snapshot): `generate_splits_multiple_domains`, `generate_splits_single_domain_returns_empty`, `generate_splits_empty_identities_returns_empty`, `generate_splits_deduplicates_domains`, `generate_splits_case_insensitive_domains`, `generate_splits_conflicting_short_names_uses_full_domain`, `generate_splits_skips_malformed_email`, `generate_splits_fastmail_plus_o365`, `seed_creates_splits_when_no_config`, `seed_skips_when_splits_exist`, `seed_skips_single_domain`. Let `E0061` compile errors confirm completeness.

- [ ] **Step 4: Run to verify GREEN**

Run: `cargo test 2>&1 | tail -5`
Expected: all tests pass, 0 failed.

- [ ] **Step 5: Lint, format, commit**

```bash
cargo clippy -- -D warnings && cargo fmt
git add src/splits.rs src/main.rs
git commit -m "Tag auto-seeded splits with the seeding account"
```

---

### Task 4: Scope the four backend consumers

**Files:**
- Modify: `src/routes.rs` (`list_splits` ~1108, `create_split` ~1116, `update_split` ~1139, `split_counts` ~1008, `list_emails` split-filter block ~466)
- Modify: `src/prefetch.rs` (`fetch_split_counts` ~669)

**Interfaces:**
- Consumes: `SplitsConfig::scoped_to` (Task 2); existing `resolve_account_id(state, Option<&str>) -> Result<String, Error>` at routes.rs:275.
- Produces: `GET /api/splits?account=X` returns only X's + untagged splits; reads and writes both reject an unknown `account` with 400 (a typo'd read would otherwise silently return only the untagged splits, indistinguishable from an account with no tagged splits). No new Rust symbols.

This task is handler glue over already-tested logic; the behavioral tests are Task 2's unit tests plus Task 6's live E2E verification. It must compile clean and leave the whole suite green.

- [ ] **Step 1: `list_splits` honors `?account=`** — replace the handler and add its params struct above it:

```rust
#[derive(Deserialize)]
struct ListSplitsParams {
    account: Option<String>,
}

async fn list_splits(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListSplitsParams>,
) -> Result<impl IntoResponse, Error> {
    // No ?account= → full list (management/debugging view). The UI always
    // sends the active account via the api() helper. When an account IS
    // given, validate it — a typo would otherwise silently compute against
    // the untagged-only split list instead of 400ing.
    if let Some(ref acct) = params.account {
        let reg = state.accounts.read().await;
        ensure_known_account(&reg, acct)?;
    }

    let config = splits::load_splits(
        &state.splits_config_path,
        std::env::var("SUPERVILLAIN_SPLITS").ok().as_deref(),
    );
    Ok(Json(serde_json::json!(
        config.scoped_to(params.account.as_deref()).splits
    )))
}
```

Also update the section banner comment above the CRUD handlers (lines ~993–999, "These four handlers … are GLOBAL …") to:

```rust
// =============================================================================
// Splits CRUD
//
// Definitions live in the single ~/.config/supervillain/splits.json, but
// each split may be tagged with an owning account. Reads (`list_splits`)
// scope to ?account=; writes validate the tag against the registry.
// `/api/split-counts` and `/api/emails?split_id=` scope to the resolved
// account before counting/filtering.
// =============================================================================
```

- [ ] **Step 2: `list_emails` scopes before `filter_by_split`** — the split-filter block becomes:

```rust
    // Apply split filtering, scoped to this account's splits so "primary"
    // means "not matching any of *this account's* splits".
    if let Some(ref split_id) = params.split_id {
        let account_id = resolve_account_id(&state, params.account.as_deref()).await?;
        let config = splits::load_splits(
            &state.splits_config_path,
            std::env::var("SUPERVILLAIN_SPLITS").ok().as_deref(),
        )
        .scoped_to(Some(&account_id));
        emails = splits::filter_by_split(emails, split_id, &config);
        emails.truncate(limit);
    }
```

- [ ] **Step 3: `split_counts` scopes once, reuses the resolved id** — resolve the account before the empty-check and drop the inner resolution in the cacheable branch:

```rust
    let account_id = resolve_account_id(&state, params.account.as_deref()).await?;
    let config = splits::load_splits(
        &state.splits_config_path,
        std::env::var("SUPERVILLAIN_SPLITS").ok().as_deref(),
    )
    .scoped_to(Some(&account_id));
    if config.splits.is_empty() {
        return Ok(Json(serde_json::json!({})));
    }
```

and in the cacheable branch, replace `let id = resolve_account_id(&state, params.account.as_deref()).await?;` with `let id = account_id.clone();` (the rest of the branch is unchanged).

- [ ] **Step 4: prefetch warmer scopes** — in `src/prefetch.rs` `fetch_split_counts`, chain onto the existing load:

```rust
    let config = crate::splits::load_splits(
        &state.splits_config_path,
        std::env::var("SUPERVILLAIN_SPLITS").ok().as_deref(),
    )
    .scoped_to(Some(account_id));
```

- [ ] **Step 5: create/update validate the tag** — in `create_split`, after the duplicate-id check:

```rust
    // Reject typos early: a split tagged to an unknown account would
    // silently never render anywhere.
    if let Some(ref acct) = new_split.account {
        let reg = state.accounts.read().await;
        if !reg.account_configs.contains_key(acct) {
            return Err(Error::BadRequest(format!("Unknown account '{acct}'")));
        }
    }
```

In `update_split`, insert the same block (with `updated.account`) after the `let mut config = …` load and before the `find`.

- [ ] **Step 6: Full suite GREEN**

Run: `cargo test 2>&1 | tail -5`
Expected: all tests pass, 0 failed.

- [ ] **Step 7: Lint, format, commit**

```bash
cargo clippy -- -D warnings && cargo fmt
git add src/routes.rs src/prefetch.rs
git commit -m "Scope split listing, filtering, and counts to the active account"
```

---

### Task 5: Frontend — account-scoped loads and tagged creates

**Files:**
- Modify: `src/routes.rs` (tests module, after `api_helper_excludes_settings_from_account_param` at ~1688)
- Modify: `static/app.js` (`loadSplits` ~797, `selectAccount` ~764, `saveSplit` ~3362)

**Interfaces:**
- Consumes: `GET /api/splits?account=` (Task 4); existing `api()` helper which auto-appends `?account=` for paths matching `ACCOUNT_SCOPED_API` (which already includes `splits`).
- Produces: nothing consumed by later tasks.

- [ ] **Step 1: Write the failing tripwire tests** — this codebase pins required frontend patterns with APP_JS substring tests (see `api_helper_excludes_settings_from_account_param`). Add after it:

```rust
    #[test]
    fn load_splits_goes_through_account_scoped_api_helper() {
        // A raw fetch skips ?account= and renders every account's tabs.
        // NOTE: assert on `const splits = await api(...)`, NOT on
        // `state.splits = await api(...)` — loadSplits must await into a
        // local and only assign state after the stale-response guard, so
        // a substring pinning direct assignment would force removal of
        // the guards (roborev 271/272/275).
        assert!(
            APP_JS.contains("const splits = await api('GET', '/splits')"),
            "loadSplits must use the api() helper so ?account= is appended"
        );
        assert!(
            !APP_JS.contains("fetch('/api/splits')"),
            "loadSplits must not bypass api() with a raw fetch"
        );
    }

    #[test]
    fn load_splits_guards_against_stale_account_switch() {
        // On rapid account switches, account A's in-flight response can
        // land after B's and overwrite state.splits while B is active.
        let start = APP_JS
            .find("async function loadSplits()")
            .expect("loadSplits must exist");
        let rest = &APP_JS[start..];
        let end = rest.find("\n}").expect("loadSplits must close");
        let body = &rest[..end];
        assert!(
            body.matches("state.currentAccount?.id !== accountId")
                .count()
                >= 2,
            "loadSplits must discard BOTH a stale success and a stale failure"
        );
    }

    #[test]
    fn select_account_reloads_splits() {
        // Tab sets differ per account; switching must rebuild the row.
        let start = APP_JS
            .find("function selectAccount")
            .expect("selectAccount must exist");
        let body = &APP_JS[start..start + 1500];
        assert!(
            body.contains("loadSplits()"),
            "selectAccount must call loadSplits()"
        );
    }

    #[test]
    fn save_split_tags_current_account() {
        assert!(
            APP_JS.contains("account: state.currentAccount?.id"),
            "saveSplit must scope new splits to the active account"
        );
    }
```

- [ ] **Step 2: Run to verify RED**

Run: `cargo test -- load_splits select_account_reloads_splits save_split_tags 2>&1 | tail -10`
Expected: 4 test failures (assertion failures, not compile errors).

- [ ] **Step 3: Edit `static/app.js`** — three changes:

`loadSplits` (switch to the api() helper AND keep/add the stale guards —
the account captured before the await must still be active before any
state mutation, on both the success and failure paths):

```js
async function loadSplits() {
    const accountId = state.currentAccount?.id;
    try {
        const splits = await api('GET', '/splits');
        if (state.currentAccount?.id !== accountId) return; // stale response guard
        state.splits = splits;
        renderSplitTabs();
        loadSplitCounts();
    } catch (err) {
        // Stale failure guard: a request from the previous account erroring
        // late must not wipe the new account's already-loaded splits.
        if (state.currentAccount?.id !== accountId) return;
        console.warn('Failed to load splits:', err);
        state.splits = [];
    }
}
```

`selectAccount` (append one call after `loadIdentities();`):

```js
    renderAccounts();
    loadMailboxes();
    loadIdentities();
    // Tab sets are per-account now; rebuild the split row (also refreshes
    // counts via loadSplitCounts).
    loadSplits();
```

`saveSplit` (add the account field to the POST body):

```js
        await api('POST', '/splits', {
            id,
            name,
            filters: [filter],
            match_mode: 'any',
            // New splits belong to the account being viewed; hand-edit
            // splits.json to make one global.
            account: state.currentAccount?.id,
        });
```

(`undefined` is dropped by JSON.stringify, so with no current account the split is created untagged.)

- [ ] **Step 4: Run to verify GREEN**

Run: `cargo test 2>&1 | tail -5`
Expected: all tests pass, 0 failed.

- [ ] **Step 5: Lint, format, commit**

```bash
cargo clippy -- -D warnings && cargo fmt
git add src/routes.rs static/app.js
git commit -m "Frontend: per-account split tabs and account-tagged creates"
```

---

### Task 6: CHANGELOG, live-file migration, deploy, E2E verify

Performed by the orchestrating session (touches the live config; needs judgment), not a subagent.

**Files:**
- Modify: `CHANGELOG.md` (entry matching the file's existing format)
- Modify (live, not in repo): `~/.config/supervillain/splits.json`

- [ ] **Step 1: CHANGELOG entry** — read the top of `CHANGELOG.md` and add an entry in its established format: splits can be tagged with an owning account; tabs, counts, and Primary are computed per account; untagged splits remain global.

- [ ] **Step 2: Commit**

```bash
git add CHANGELOG.md
git commit -m "Changelog: account-scoped splits"
```

- [ ] **Step 3: Migrate the live splits.json** (backup first; the server still running the old binary is fine — serde without `deny_unknown_fields` ignores the new `account` field, so the old binary tolerates the migrated file. The `jq . > /dev/null` check below guards against jq producing malformed output, since `load_splits` silently falls back to defaults on parse failure):

```bash
cp ~/.config/supervillain/splits.json ~/.config/supervillain/splits.json.bak-$(date +%Y%m%d)
jq '.splits |= map(
      select(.id != "itga")
      | .account = ({"aristoi":"aristoi","mattcoburn":"aristoi","mattgpt":"aristoi",
                     "gmail":"gmail","aristotle":"outlook-aristotle"}[.id] // .account)
    )' ~/.config/supervillain/splits.json > /tmp/splits.new.json
jq . /tmp/splits.new.json > /dev/null && mv /tmp/splits.new.json ~/.config/supervillain/splits.json
```

- [ ] **Step 4: Deploy**

```bash
./scripts/upgrade.sh
```

- [ ] **Step 5: E2E verify against the live server**

```bash
curl -s 'http://127.0.0.1:8000/api/splits?account=gmail' | jq -r '.[].id'
# expected: gmail
curl -s 'http://127.0.0.1:8000/api/splits?account=aristoi' | jq -r '.[].id'
# expected: aristoi, mattcoburn, mattgpt
curl -s 'http://127.0.0.1:8000/api/splits?account=outlook-aristotle' | jq -r '.[].id'
# expected: aristotle
curl -s 'http://127.0.0.1:8000/api/splits' | jq -r '.[].id'
# expected: all five (management view)
curl -s -X POST 'http://127.0.0.1:8000/api/splits' -H 'Content-Type: application/json' \
  -d '{"id":"typo-test","name":"T","filters":[],"match_mode":"any","account":"nope"}'
# expected: 400 Unknown account 'nope'
```

Then confirm in the browser: gmail account shows only Primary + gmail tabs and Primary is populated; aristoi shows aristoi/mattcoburn/mattgpt tabs.

- [ ] **Step 6: roborev** — per project workflow, request roborev review of the new commits.

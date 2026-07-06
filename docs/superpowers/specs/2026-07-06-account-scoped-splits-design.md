# Account-Scoped Splits — Design

**Date:** 2026-07-06
**Status:** Approved

## Problem

`splits.json` is a single global list. Every split tab renders on every
account, so the gmail account shows aristoi/mattcoburn/mattgpt/aristotle
tabs that can never match, and — worse — the `gmail` split (`*@gmail.com`)
matches essentially all mail on the gmail account, so its Primary tab
(defined as "not matching any split") is nearly empty. Splits need to be
associated with the account whose mail they filter.

Current account → split mapping, derived from live identities:

| Split | Pattern | Owning account |
|---|---|---|
| aristoi, mattgpt, mattcoburn | `*@aristoi.ai`, `*@mattgpt.ai`, `*@mattcoburn.ai` | `aristoi` (Fastmail) |
| gmail | `*@gmail.com` | `gmail` |
| aristotle | `*@aristotle.ai` | `outlook-aristotle` |
| itga | `*@itga*` | stale — delete |

## Decision

Add an explicit optional `account` field to each split (approach A).
Auto-derivation from identities at runtime (approach B) was rejected:
it breaks for `from`/`subject`/`calendar` filters, cannot be overridden
when derivation guesses wrong (`itga` proved it will), and forwarded-mail
setups (O365 relayed through Fastmail) deliberately have To: domains that
don't match the account. Per-account splits files / INI embedding
(approach C) were rejected as heavier migrations that kill global splits.

## 1. Data model

`SplitInbox` gains:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub account: Option<String>,
```

- Value is a config-section account id (`"aristoi"`, `"gmail"`, …).
- `Some(id)` → the split exists only for that account.
- `None`/absent → the split applies to **all** accounts. Every existing
  splits.json stays valid, and cross-account splits (e.g. Calendar)
  remain possible.
- A split tagged to a since-deleted account is never listed anywhere; it
  stays in the JSON for hand-editing. This is accepted (personal app,
  hand-editable file).
- Split ids remain globally unique across accounts.

## 2. Backend scoping

New method:

```rust
impl SplitsConfig {
    /// Splits visible to `account`: untagged splits plus splits tagged
    /// with exactly this account. `None` returns everything (management
    /// view / no-param requests).
    pub fn scoped_to(&self, account: Option<&str>) -> SplitsConfig
}
```

Applied at four points, always with the **resolved** account id (query
param falling back to the default account, mirroring `resolve_session`):

1. **`GET /api/splits`** (`list_splits`) — gains an `account` query
   param; returns the scoped list. No param → full list.
2. **`GET /api/emails?split_id=`** (`list_emails`) — scopes the config
   before `splits::filter_by_split`. Correctness fix: **"primary" becomes
   "not matching any of this account's splits"**.
3. **`GET /api/split-counts`** (`split_counts`) and the **prefetch
   warmer** — both scope before `compute_split_counts`; an empty scoped
   list early-returns `{}`. Both callers pass the already-scoped config
   so the shared function stays drift-free.
4. **`POST /api/splits` / `PUT /api/splits/{id}`** — accept the field in
   the request body. If `account` is `Some`, validate it exists in the
   account registry; unknown id → 400.

`filter_by_split` / `matches_any_split` themselves stay account-unaware;
callers hand them a pre-scoped config.

## 3. Seeding

`generate_splits_from_identities` takes the seeding account's id and tags
every generated split with it. The once-when-empty, default-account-only
semantics are unchanged. The `splits.rs` module doc — which currently
asserts splits cannot be account-scoped — is rewritten to describe the
new model.

## 4. Frontend (static/app.js)

- `loadSplits()` switches from raw `fetch('/api/splits')` to the `api()`
  wrapper so `?account=` is appended (the `ACCOUNT_SCOPED_API` regex
  already includes `splits`).
- `selectAccount()` calls `loadSplits()` so the tab row rebuilds when
  switching accounts.
- `saveSplit()` includes `account: state.currentAccount?.id` in the POST
  body — new splits are born scoped to the account being viewed. Making
  a split global is a hand-edit of splits.json (no UI checkbox; YAGNI).

## 5. Migration of the live file

One-time hand edit of `~/.config/supervillain/splits.json`, done
alongside the upgrade/restart so the stale-config tripwire doesn't fire
mid-session:

- Tag `aristoi`, `mattcoburn`, `mattgpt` → `"account": "aristoi"`
- Tag `gmail` → `"account": "gmail"`
- Tag `aristotle` → `"account": "outlook-aristotle"`
- Delete `itga` (stale)

No automatic migration code: untagged splits already mean "current
behavior" for any other install.

## 6. Testing

Behavioral red/green TDD:

- `scoped_to`: tagged-match kept, tagged-other dropped, untagged always
  kept, `None` account returns all.
- Serde: round-trip with `account` set; back-compat parse of
  account-less JSON; `None` not serialized.
- Seeding: generated splits carry the seeding account id.
- Primary semantics: with a config scoped to account X, mail matching
  another account's split lands in X's primary.
- Route-level: `list_splits` honors `?account=`; create/update reject
  unknown account ids.

`cargo clippy -- -D warnings` + `cargo fmt` before commit; roborev after.

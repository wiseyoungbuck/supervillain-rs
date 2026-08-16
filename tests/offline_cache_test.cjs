// Behavioral tests for offline mail (kata 2chc).
//
// The mobile client's only offline dataset is its own localStorage snapshot —
// the service worker deliberately bypasses /api/ — so these tests extract the
// real snapshot writer, the real restore, the real offline gate, and the real
// init() from static/mobile/app.js and drive them against a fake localStorage
// and a mock DOM. They pin behavior (what lands in the blob, what renders, what
// stops touching the server), not source shape.
//
// Scope pins as much as coverage pins: offline is READ-ONLY. Blocked actions
// must be blocked, never queued — a test that accepted a queued archive would
// be waving through the sync engine this ticket explicitly refuses to build.

const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const MOBILE = fs.readFileSync(
    path.join(__dirname, '..', 'static', 'mobile', 'app.js'), 'utf8');
const API_JS = fs.readFileSync(
    path.join(__dirname, '..', 'static', 'api.js'), 'utf8');

// Slice a declaration out of a bundle: from the declaration to the first
// column-0 closing brace. Same assumption as every other extract-and-eval
// suite here (tests/refill_test.cjs, tests/email_refresh_test.cjs).
function extract(source, declaration) {
    const start = source.indexOf(declaration);
    assert.notStrictEqual(start, -1, `${declaration} must exist`);
    const close = source.indexOf('\n}', start);
    assert.notStrictEqual(close, -1, `${declaration} must close`);
    return source.slice(start, close + 2);
}

// Single-line `const NAME = ...;` declarations, pulled from the bundle so the
// tests run against the shipped values rather than copies that can drift.
function extractConst(name) {
    const match = MOBILE.match(new RegExp(`^const ${name} = .*$`, 'm'));
    assert.ok(match, `const ${name} must exist`);
    return match[0];
}

// A top-level `document.getElementById('x').addEventListener(...)` statement,
// closed by a column-0 `});` — extract() would cut the `);` off.
function extractListener(prefix) {
    const start = MOBILE.indexOf(prefix);
    assert.notStrictEqual(start, -1, `listener ${prefix} must exist`);
    const close = MOBILE.indexOf('\n});', start);
    assert.notStrictEqual(close, -1, `listener ${prefix} must close`);
    return MOBILE.slice(start, close + 4);
}

// Multi-line `const NAME = [ ... ];` declarations.
function extractConstArray(name) {
    const start = MOBILE.indexOf(`const ${name} = [`);
    assert.notStrictEqual(start, -1, `const ${name} must exist`);
    const close = MOBILE.indexOf('\n];', start);
    assert.notStrictEqual(close, -1, `const ${name} must close`);
    return MOBILE.slice(start, close + 3);
}

// ============================================================================
// Mock DOM / storage
// ============================================================================

function makeElement(id) {
    const classes = new Set();
    const listeners = {};
    return {
        id,
        textContent: '',
        innerHTML: '',
        value: '',
        scrollTop: 0,
        disabled: false,
        readOnly: false,
        style: {},
        listeners,
        addEventListener(type, fn) { listeners[type] = fn; },
        classList: {
            add: (c) => { classes.add(c); },
            remove: (c) => { classes.delete(c); },
            contains: (c) => classes.has(c),
            toggle: (c, on) => {
                const next = on === undefined ? !classes.has(c) : !!on;
                if (next) classes.add(c); else classes.delete(c);
                return next;
            },
        },
        setAttribute() {},
        removeAttribute() {},
        focus() {},
    };
}

// Auto-vivifying: any id the code under test reaches for exists, so a mock
// that lags behind the real index.html can't turn a behavior failure into a
// null-dereference.
function makeDocument() {
    const els = new Map();
    return {
        getElementById(id) {
            if (!els.has(id)) els.set(id, makeElement(id));
            return els.get(id);
        },
    };
}

function makeStorage(initial = {}) {
    const data = { ...initial };
    const reads = [];
    return {
        getItem(k) { reads.push(k); return (k in data ? data[k] : null); },
        setItem(k, v) { data[k] = String(v); },
        removeItem(k) { delete data[k]; },
        raw: data,
        reads,
    };
}

// The real error taxonomy (static/api.js): ApiAuthError extends ApiError, and
// the offline path must be able to tell them apart.
function loadErrorTaxonomy() {
    const code = [
        extract(API_JS, 'class ApiError extends Error {'),
        extract(API_JS, 'class ApiAuthError extends ApiError {'),
        'return { ApiError, ApiAuthError };',
    ].join('\n');
    // eslint-disable-next-line no-new-func
    return new Function(code)();
}

const { ApiError, ApiAuthError } = loadErrorTaxonomy();

// ============================================================================
// Harness
// ============================================================================

function email(id, overrides = {}) {
    return {
        id,
        subject: 'Subject ' + id,
        preview: 'preview',
        from: [{ name: 'Sender', email: 'sender@example.com' }],
        receivedAt: '2026-08-16T10:00:00Z',
        isUnread: false,
        isFlagged: false,
        ...overrides,
    };
}

function body(id, size = 8) {
    return { ...email(id), textBody: 'x'.repeat(size), htmlBody: null };
}

function makeHarness({ storage = {}, accountsResult, stateOverrides = {} } = {}) {
    const calls = [];
    const localStorage = makeStorage(storage);
    const document = makeDocument();
    const state = {
        accounts: [],
        currentAccount: null,
        api: null,
        mailboxes: [],
        currentMailbox: null,
        currentSplit: 'all',
        splits: [],
        splitCounts: {},
        emails: [],
        loading: false,
        loadAbort: null,
        screen: 'list',
        currentEmailId: null,
        listScrollTop: 0,
        emailCache: {},
        undoStack: [],
        identities: [],
        pendingAttachments: [],
        searchQuery: '',
        emailsFetchedAt: null,
        sending: false,
        composeSession: 0,
        offlineMode: false,
        ...stateOverrides,
    };

    const log = (name) => (...args) => { calls.push({ name, args }); };
    const api = async (method, path) => {
        calls.push({ name: 'api', args: [method, path] });
        return null;
    };
    // accountsResult may be a function to script per-attempt results (e.g. a
    // fetch that fails once, then succeeds after reconnect).
    const loadAccounts = async () => {
        calls.push({ name: 'loadAccounts', args: [] });
        const result = typeof accountsResult === 'function' ? accountsResult() : accountsResult;
        if (result instanceof Error) throw result;
        state.accounts = result || [];
    };

    const code = [
        extractConst('PAGE_SIZE'),
        extractConst('BODY_CACHE_LIMIT'),
        extractConst('MOBILE_STATE_KEY'),
        extractConst('STATE_MAX_AGE_MS'),
        extractConst('SNAPSHOT_BODY_LIMIT'),
        extractConst('SNAPSHOT_MAX_CHARS'),
        extractConst('OFFLINE_BANNER_STALE'),
        extractConst('OFFLINE_BANNER_CACHED'),
        extractConstArray('OFFLINE_DISABLED_CONTROLS'),
        'let initInFlight = false;',
        'let reconnectedDuringBoot = false;',
        'let bootIncomplete = false;',
        'let undoToastEntry = null;',
        extract(MOBILE, 'function cacheEmail('),
        extract(MOBILE, 'function snapshotBodies('),
        extract(MOBILE, 'function serializeSnapshot('),
        extract(MOBILE, 'function persistState('),
        extract(MOBILE, 'function restoreFromSnapshot('),
        extract(MOBILE, 'function setOfflineBanner('),
        extract(MOBILE, 'function setOfflineMode('),
        extract(MOBILE, 'function offlineBlocked('),
        extract(MOBILE, 'async function emailAction('),
        extract(MOBILE, 'function selectAccount('),
        extract(MOBILE, 'async function performUndo('),
        extract(MOBILE, 'async function rsvpToEvent('),
        extract(MOBILE, 'function submitSearch('),
        extract(MOBILE, 'function clearSearch('),
        extract(MOBILE, 'async function sendComposedEmail('),
        extract(MOBILE, 'async function renderScreenDetail('),
        extract(MOBILE, 'async function init('),
        extract(MOBILE, 'function handleOffline('),
        extract(MOBILE, 'function handleOnline('),
        extract(MOBILE, 'const pullToRefreshRecognizer = {'),
        extractListener("document.getElementById('detail-attachments').addEventListener('click'"),
        extractListener("document.getElementById('bottom-nav').addEventListener('click'"),
        `return { persistState, restoreFromSnapshot, snapshotBodies, pullToRefreshRecognizer,
                  handleOffline, handleOnline, selectAccount, performUndo, rsvpToEvent,
                  serializeSnapshot, setOfflineMode, offlineBlocked, cacheEmail,
                  emailAction, submitSearch, clearSearch, sendComposedEmail,
                  renderScreenDetail, init,
                  SNAPSHOT_BODY_LIMIT, SNAPSHOT_MAX_CHARS,
                  OFFLINE_BANNER_CACHED, OFFLINE_BANNER_STALE,
                  OFFLINE_DISABLED_CONTROLS };`,
    ].join('\n');

    // eslint-disable-next-line no-new-func
    const fns = new Function(
        'state', 'document', 'localStorage', 'navigator', 'console', 'Screen',
        'AbortController', 'ApiError', 'ApiAuthError',
        'makeApi', 'connectedAccounts', 'abortListLoad', 'renderAccountButton',
        'renderEmailList', 'showStatus', 'showToast', 'showError',
        'loadAccounts', 'hideBootSplash', 'selectAccount',
        'loadIdentities', 'loadSplits', 'loadMailboxes', 'loadEmails',
        'adjustSplitCounts', 'pushUndo', 'hideUndoToast', 'capUndoStack',
        'prefetchAdjacentEmails', 'renderEmailDetail',
        'renderEmailDetailPartial', 'renderDetailActionBar',
        'setComposeSending', 'doSendComposedEmail', 'cancelAutosave',
        'finishPullRefresh', 'downloadAllAttachments',
        'hideAccountPicker', 'updateCalendarCard', 'selectMailbox',
        'setScreen', 'closeSearchBar', 'history',
        code,
    )(
        state, document, localStorage, { onLine: true },
        { warn: log('console.warn'), info: log('console.info') },
        { LIST: 'list', DETAIL: 'detail', COMPOSE: 'compose' },
        AbortController, ApiError, ApiAuthError,
        () => api,
        () => state.accounts.filter(a => a.authStatus !== 'pending'),
        () => { calls.push({ name: 'abortListLoad', args: [] }); state.loadAbort = new AbortController(); },
        log('renderAccountButton'),
        log('renderEmailList'),
        log('showStatus'),
        log('showToast'),
        (context, err) => { calls.push({ name: 'showError', args: [context, err] }); },
        loadAccounts,
        log('hideBootSplash'),
        log('selectAccount'),
        log('loadIdentities'),
        async () => { calls.push({ name: 'loadSplits', args: [] }); },
        async () => { calls.push({ name: 'loadMailboxes', args: [] }); },
        async () => { calls.push({ name: 'loadEmails', args: [] }); },
        log('adjustSplitCounts'),
        (action, e, index) => {
            const entry = { action, email: e, index, settled: null };
            state.undoStack.push(entry);
            return entry;
        },
        log('hideUndoToast'),
        log('capUndoStack'),
        log('prefetchAdjacentEmails'),
        log('renderEmailDetail'),
        log('renderEmailDetailPartial'),
        log('renderDetailActionBar'),
        (sending) => { state.sending = sending; calls.push({ name: 'setComposeSending', args: [sending] }); },
        async () => { calls.push({ name: 'doSendComposedEmail', args: [] }); },
        log('cancelAutosave'),
        log('finishPullRefresh'),
        log('downloadAllAttachments'),
        log('hideAccountPicker'),
        log('updateCalendarCard'),
        log('selectMailbox'),
        log('setScreen'),
        log('closeSearchBar'),
        { replaceState() {} },
    );

    const snapshot = () => JSON.parse(localStorage.getItem('supervillain_mobile_state_v1'));
    const names = () => calls.map(c => c.name);
    return { ...fns, state, calls, names, localStorage, document, snapshot };
}

// A snapshot blob shaped exactly as persistState writes it.
function savedBlob(overrides = {}) {
    return JSON.stringify({
        accountId: 'acct-1',
        accountEmail: 'me@example.com',
        accounts: [{ id: 'acct-1', email: 'me@example.com', provider: 'fastmail' }],
        mailboxRole: 'inbox',
        mailboxId: 'mb-inbox',
        splitId: 'all',
        searchQuery: '',
        emails: [email('e1'), email('e2')],
        bodies: [body('e1')],
        listScrollTop: 0,
        savedAt: Date.now(),
        ...overrides,
    });
}

const KEY = 'supervillain_mobile_state_v1';

// ============================================================================
// Snapshot coverage
// ============================================================================

test('persistState snapshots the accounts list, the header list and opened bodies', () => {
    const h = makeHarness();
    h.state.accounts = [{ id: 'acct-1', email: 'me@example.com', provider: 'fastmail' }];
    h.state.currentAccount = h.state.accounts[0];
    h.state.currentMailbox = { id: 'mb-inbox', role: 'inbox' };
    h.state.emails = [email('e1'), email('e2')];
    h.cacheEmail(body('e1'));

    h.persistState();

    const snap = h.snapshot();
    assert.deepStrictEqual(snap.accounts, h.state.accounts,
        'the accounts list must be cached — an offline cold start has no /api/accounts to fetch');
    assert.deepStrictEqual(snap.emails.map(e => e.id), ['e1', 'e2']);
    assert.deepStrictEqual(snap.bodies.map(b => b.id), ['e1'],
        'opened message bodies must be cached for offline read');
    assert.strictEqual(snap.bodies[0].textBody, 'x'.repeat(8));
});

test('snapshot keeps only the most recent SNAPSHOT_BODY_LIMIT bodies, oldest evicted first', () => {
    const h = makeHarness();
    const limit = h.SNAPSHOT_BODY_LIMIT;
    assert.strictEqual(limit, 20, 'the ticket sizes the body cache at ~20 opened messages');
    h.state.accounts = [{ id: 'acct-1' }];
    h.state.currentAccount = { id: 'acct-1', email: 'me@example.com' };
    h.state.currentMailbox = { id: 'mb-inbox', role: 'inbox' };
    for (let i = 0; i < limit + 5; i++) h.cacheEmail(body('e' + i));

    h.persistState();

    const ids = h.snapshot().bodies.map(b => b.id);
    assert.strictEqual(ids.length, limit);
    assert.strictEqual(ids[0], 'e5', 'the five oldest opens must be evicted, not the newest');
    assert.strictEqual(ids[ids.length - 1], 'e' + (limit + 4));
});

test('opened bodies also evict oldest-first when more than the limit were opened', () => {
    // The cap test above exercises the prefetched arm of snapshotBodies; this
    // one pins the opened arm, where an eviction that kept the OLDEST opens
    // would ship a snapshot of last week's reading and drop today's.
    const h = makeHarness();
    const limit = h.SNAPSHOT_BODY_LIMIT;
    h.state.accounts = [{ id: 'acct-1' }];
    h.state.currentAccount = { id: 'acct-1', email: 'me@example.com' };
    h.state.currentMailbox = { id: 'mb-inbox', role: 'inbox' };
    for (let i = 0; i < limit + 5; i++) h.cacheEmail(body('e' + i), { opened: true });

    h.persistState();

    const ids = h.snapshot().bodies.map(b => b.id);
    assert.strictEqual(ids.length, limit);
    assert.strictEqual(ids[0], 'e5', 'the five oldest opens must be evicted, not the newest');
    assert.strictEqual(ids[ids.length - 1], 'e' + (limit + 4));
});

test('prefetched neighbours never crowd opened mail out of the snapshot', () => {
    // roborev 517: prefetchAdjacentEmails caches up to 3 unread neighbours per
    // open, so a purely recency-ordered tail would fill the 20 body slots with
    // speculative fodder and evict the mail the user actually read today —
    // which then reports "not opened while connected" offline.
    const h = makeHarness();
    h.state.accounts = [{ id: 'acct-1' }];
    h.state.currentAccount = { id: 'acct-1', email: 'me@example.com' };
    h.state.currentMailbox = { id: 'mb-inbox', role: 'inbox' };
    for (let i = 0; i < 20; i++) h.cacheEmail(body('open' + i), { opened: true });
    for (let i = 0; i < 20; i++) h.cacheEmail(body('pre' + i));

    h.persistState();

    const ids = h.snapshot().bodies.map(b => b.id);
    assert.strictEqual(ids.length, 20);
    assert.ok(ids.every(id => id.startsWith('open')),
        `opened mail must own every slot when it fills them, got ${ids.join(',')}`);
});

test('prefetched bodies fill the snapshot slots opened mail leaves free', () => {
    const h = makeHarness();
    h.state.accounts = [{ id: 'acct-1' }];
    h.state.currentAccount = { id: 'acct-1', email: 'me@example.com' };
    h.state.currentMailbox = { id: 'mb-inbox', role: 'inbox' };
    for (let i = 0; i < 5; i++) h.cacheEmail(body('open' + i), { opened: true });
    for (let i = 0; i < 30; i++) h.cacheEmail(body('pre' + i));

    h.persistState();

    const ids = h.snapshot().bodies.map(b => b.id);
    assert.strictEqual(ids.length, 20);
    for (let i = 0; i < 5; i++) {
        assert.ok(ids.includes('open' + i), `opened body open${i} must survive`);
    }
    assert.ok(ids.includes('pre29'), 'the newest prefetched bodies fill the remainder');
    assert.ok(!ids.includes('pre0'), 'the oldest prefetched bodies are dropped first');
});

test('re-caching an already-cached id at capacity evicts nothing', () => {
    // roborev 526: a re-insert replaces its own key — it does not grow the
    // cache — so running the eviction branch would shrink the cache by one
    // unrelated body for no capacity gain.
    const h = makeHarness();
    for (let i = 0; i < 50; i++) h.cacheEmail(body('e' + i));

    h.cacheEmail(body('e10'));

    assert.strictEqual(Object.keys(h.state.emailCache).length, 50,
        'replacing an existing key must not shrink the cache');
    assert.ok(h.state.emailCache.e0, 'no victim may be evicted for a re-insert');
});

test('opening a prefetched message promotes it to opened for snapshot purposes', async () => {
    const h = makeHarness();
    h.state.emails = [email('e1')];
    h.state.currentEmailId = 'e1';
    h.state.api = async () => null;
    h.cacheEmail(body('e1'));

    await h.renderScreenDetail('e1');

    assert.strictEqual(h.state.emailCache.e1.openedByUser, true,
        'a prefetched body the user then opens must claim an opened slot');
});

test('serializeSnapshot drops bodies oldest-first until the blob fits the cap', () => {
    const h = makeHarness();
    const snap = {
        accountId: 'acct-1',
        emails: [email('e1')],
        bodies: [body('old', 400), body('mid', 400), body('new', 400)],
        savedAt: Date.now(),
    };
    const json = h.serializeSnapshot(snap, 1000);
    assert.ok(json.length <= 1000, `blob must fit the cap, got ${json.length}`);
    const kept = JSON.parse(json).bodies.map(b => b.id);
    assert.ok(!kept.includes('old'), 'the oldest body must go first');
    assert.ok(kept.includes('new'), 'the newest body must survive longest');
});

test('serializeSnapshot keeps the list rows even when every body has to go', () => {
    const h = makeHarness();
    const snap = {
        accountId: 'acct-1',
        emails: [email('e1')],
        bodies: [body('a', 5000), body('b', 5000)],
        savedAt: Date.now(),
    };
    const parsed = JSON.parse(h.serializeSnapshot(snap, 600));
    assert.deepStrictEqual(parsed.bodies, [],
        'bodies are the sacrificial payload; the list is what a cold start needs most');
    assert.deepStrictEqual(parsed.emails.map(e => e.id), ['e1']);
});

test('the default blob cap stays under the localStorage quota', () => {
    const h = makeHarness();
    assert.ok(h.SNAPSHOT_MAX_CHARS >= 1024 * 1024 && h.SNAPSHOT_MAX_CHARS <= 3 * 1024 * 1024,
        `cap must sit in the 1–3 MB band against a ~5 MB quota, got ${h.SNAPSHOT_MAX_CHARS}`);
});

test('persistState carries the previous accounts list forward when state has none', () => {
    const h = makeHarness({ storage: { [KEY]: savedBlob() } });
    h.state.currentAccount = { id: 'acct-1', email: 'me@example.com' };
    h.state.emails = [email('e1')];

    h.persistState();

    assert.deepStrictEqual(h.snapshot().accounts.map(a => a.id), ['acct-1'],
        'a degraded session that never fetched accounts must not blank the cached list');
});

// ============================================================================
// Offline restore
// ============================================================================

test('offline restore seeds accounts, the header list and the cached bodies', () => {
    const h = makeHarness({ storage: { [KEY]: savedBlob() } });

    assert.strictEqual(h.restoreFromSnapshot({ offline: true }), true);
    assert.deepStrictEqual(h.state.accounts.map(a => a.id), ['acct-1'],
        'the cached accounts list is what makes an offline cold start possible');
    assert.strictEqual(h.state.currentAccount.id, 'acct-1');
    assert.deepStrictEqual(h.state.emails.map(e => e.id), ['e1', 'e2']);
    assert.deepStrictEqual(Object.keys(h.state.emailCache), ['e1'],
        'cached bodies must land in the body cache so an offline open renders');
    assert.ok(h.names().includes('renderEmailList'), 'the cached list must be painted');
});

test('offline restore skips the refresh cascade — there is no server to ask', () => {
    const h = makeHarness({ storage: { [KEY]: savedBlob() } });
    h.restoreFromSnapshot({ offline: true });
    assert.ok(!h.names().includes('loadMailboxes'));
    assert.ok(!h.names().includes('loadEmails'));
});

test('a stale snapshot is discarded rather than restored offline', () => {
    const old = Date.now() - (25 * 60 * 60 * 1000);
    const h = makeHarness({ storage: { [KEY]: savedBlob({ savedAt: old }) } });
    assert.strictEqual(h.restoreFromSnapshot({ offline: true }), false);
    assert.strictEqual(h.localStorage.getItem(KEY), null);
});

// ============================================================================
// Offline mode: banner + disabled actions
// ============================================================================

test('offline mode raises the cached-mail banner and disables server-touching controls', () => {
    const h = makeHarness();
    h.setOfflineMode(true);

    const banner = h.document.getElementById('offline-banner');
    assert.strictEqual(banner.textContent, 'Offline — showing cached mail');
    assert.ok(banner.classList.contains('visible'));
    for (const id of h.OFFLINE_DISABLED_CONTROLS) {
        assert.strictEqual(h.document.getElementById(id).disabled, true,
            `${id} must be disabled offline`);
    }
    for (const id of ['compose-btn', 'search-btn', 'detail-archive-btn',
        'detail-trash-btn', 'compose-send-btn']) {
        assert.ok(h.OFFLINE_DISABLED_CONTROLS.includes(id),
            `${id} touches the server and must be disabled offline`);
    }
});

test('a connectivity drop in a live session shows the STALE banner, not CACHED', () => {
    // roborev 527: the two-message split is the point of the banner — a live
    // session that loses connectivity has aging data ("data may be stale"),
    // not a read-only cached session, and telling it archive taps won't work
    // would be false. Pins the offlineMode ternary in setOfflineBanner.
    const h = makeHarness();
    h.handleOffline();

    const banner = h.document.getElementById('offline-banner');
    assert.ok(banner.classList.contains('visible'));
    assert.strictEqual(banner.textContent, h.OFFLINE_BANNER_STALE,
        'a merely-stale live session must not claim to be showing cached mail');
});

test('a degraded session keeps the CACHED banner text through handleOffline', () => {
    const h = makeHarness({ stateOverrides: { offlineMode: true } });
    h.handleOffline();

    assert.strictEqual(h.document.getElementById('offline-banner').textContent,
        h.OFFLINE_BANNER_CACHED);
});

test('leaving offline mode clears the banner and re-enables the controls', () => {
    const h = makeHarness();
    h.setOfflineMode(true);
    h.setOfflineMode(false);

    const banner = h.document.getElementById('offline-banner');
    assert.strictEqual(banner.classList.contains('visible'), false);
    for (const id of h.OFFLINE_DISABLED_CONTROLS) {
        assert.strictEqual(h.document.getElementById(id).disabled, false);
    }
});

test('archive is refused offline — not queued', async () => {
    const h = makeHarness({ stateOverrides: { offlineMode: true } });
    h.state.emails = [email('e1'), email('e2')];

    await h.emailAction('archive', 'e1');

    assert.ok(!h.names().includes('api'), 'no request may be attempted offline');
    assert.deepStrictEqual(h.state.emails.map(e => e.id), ['e1', 'e2'],
        'the list must not change optimistically for an action that never happens');
    assert.deepStrictEqual(h.state.undoStack, [],
        'nothing may be queued for later replay — offline is read-only');
    assert.ok(h.names().includes('showToast'), 'the refusal must be visible');
});

test('trash is refused offline — not queued', async () => {
    const h = makeHarness({ stateOverrides: { offlineMode: true } });
    h.state.emails = [email('e1')];
    await h.emailAction('trash', 'e1');
    assert.ok(!h.names().includes('api'));
    assert.deepStrictEqual(h.state.emails.map(e => e.id), ['e1']);
});

test('search is refused offline — the cached list stays put', () => {
    const h = makeHarness({ stateOverrides: { offlineMode: true } });
    h.document.getElementById('search-input').value = 'from:someone';

    h.submitSearch();

    assert.strictEqual(h.state.searchQuery, '');
    assert.ok(!h.names().includes('loadEmails'), 'search is a server round-trip');
});

test('send is refused offline — no lock taken, no draft posted', async () => {
    const h = makeHarness({ stateOverrides: { offlineMode: true } });
    await h.sendComposedEmail();
    assert.ok(!h.names().includes('doSendComposedEmail'));
    assert.ok(!h.names().includes('setComposeSending'));
    assert.strictEqual(h.state.sending, false);
});

test('RSVP is refused offline — cached-body buttons have no disabled styling', async () => {
    // roborev 526: the RSVP buttons render from a cached body, so they are
    // reachable and live-looking in a degraded session — the handler guard is
    // the only enforcement, and this pins it.
    const h = makeHarness({ stateOverrides: { offlineMode: true } });
    h.state.currentEmailId = 'e1';
    h.cacheEmail({ ...body('e1'), calendarEvent: { user_rsvp_status: 'needs-action' } });
    h.state.api = async (m, p) => { h.calls.push({ name: 'api', args: [m, p] }); return {}; };

    await h.rsvpToEvent('accepted');

    assert.ok(!h.names().includes('api'), 'no RSVP POST may be attempted offline');
    assert.strictEqual(h.state.emailCache.e1.calendarEvent.user_rsvp_status, 'needs-action',
        'the optimistic flip must not happen for a refused RSVP');
    assert.ok(h.names().includes('showToast'), 'the refusal must be visible');
});

test('undo is refused offline — the undo record survives for later', async () => {
    const h = makeHarness({ stateOverrides: { offlineMode: true } });
    const entry = { action: 'archive', email: email('e1'), index: 0, mailboxId: 'mb-inbox', settled: null };
    h.state.undoStack.push(entry);
    h.state.api = async (m, p) => { h.calls.push({ name: 'api', args: [m, p] }); };

    await h.performUndo();

    assert.ok(!h.names().includes('api'), 'no move-back may be attempted offline');
    assert.strictEqual(h.state.undoStack.length, 1,
        'the entry must not be popped — it is the only record of what to undo');
    assert.ok(h.names().includes('showToast'), 'the refusal must be visible');
});

test('account switching is refused offline and the picker is dismissed', () => {
    // The picker renders from the CACHED accounts list, so it opens and its
    // rows look live in a degraded session.
    const h = makeHarness({ stateOverrides: { offlineMode: true } });
    h.state.currentAccount = { id: 'acct-1', email: 'me@example.com' };

    h.selectAccount({ id: 'acct-2', email: 'other@example.com' });

    assert.strictEqual(h.state.currentAccount.id, 'acct-1',
        'the switch must not happen offline — its whole body is a reload cascade');
    assert.ok(!h.names().includes('loadMailboxes'));
    assert.ok(h.names().includes('hideAccountPicker'),
        'the modal must not stay up over the refusal toast');
    assert.ok(h.names().includes('showToast'));
});

test('bottom-nav mailbox switching is refused offline with a visible toast', () => {
    // A degraded session never fetched /mailboxes, so without the guard the
    // tap would silently no-op — the toast is the only honest answer.
    const h = makeHarness({ stateOverrides: { offlineMode: true } });
    const handler = h.document.getElementById('bottom-nav').listeners.click;
    assert.ok(handler, 'the bottom-nav click handler must be registered');

    handler({ target: { closest: () => ({ dataset: { role: 'archive' } }) } });

    assert.ok(!h.names().includes('selectMailbox'));
    assert.ok(h.names().includes('showToast'), 'the refusal must be visible');
});

test('actions are allowed again once offline mode clears', async () => {
    const h = makeHarness({ stateOverrides: { offlineMode: true } });
    h.state.currentAccount = { id: 'acct-1' };
    h.state.api = async (method, p) => { h.calls.push({ name: 'api', args: [method, p] }); };
    h.state.emails = [email('e1')];
    h.setOfflineMode(false);

    await h.emailAction('archive', 'e1');

    assert.ok(h.names().includes('api'), 'the offline gate must not outlive the offline session');
});

// ============================================================================
// Offline read
// ============================================================================

test('an opened message with a cached body renders offline without a request', async () => {
    const h = makeHarness({ stateOverrides: { offlineMode: true } });
    h.state.emails = [email('e1')];
    h.state.currentEmailId = 'e1';
    h.cacheEmail(body('e1'));

    await h.renderScreenDetail('e1');

    assert.ok(!h.names().includes('api'), 'a cache hit must never reach the network');
    assert.ok(h.names().includes('renderEmailDetail'), 'the cached body must render');
});

test('an uncached message reports honestly offline instead of firing a doomed GET', async () => {
    const h = makeHarness({ stateOverrides: { offlineMode: true } });
    h.state.emails = [email('e9')];
    h.state.currentEmailId = 'e9';

    await h.renderScreenDetail('e9');

    assert.ok(!h.names().includes('api'), 'no fetch may be attempted offline');
    const shown = h.document.getElementById('email-body').innerHTML;
    assert.match(shown, /offline/i, `body pane must explain why, got: ${shown}`);
});

// ============================================================================
// init(): restore → paint → revalidate
// ============================================================================

test('init paints the cached snapshot BEFORE fetching accounts', async () => {
    const h = makeHarness({
        storage: { [KEY]: savedBlob() },
        accountsResult: [{ id: 'acct-1', email: 'me@example.com' }],
    });

    await h.init();

    const order = h.names();
    const painted = order.indexOf('renderEmailList');
    const fetched = order.indexOf('loadAccounts');
    assert.notStrictEqual(painted, -1, 'the cached list must be painted');
    assert.ok(painted < fetched,
        `restore must precede the account fetch, got ${JSON.stringify(order)}`);
});

test('an offline cold start renders cached mail instead of dead-ending', async () => {
    const h = makeHarness({
        storage: { [KEY]: savedBlob() },
        accountsResult: new ApiError('Network error: failed to fetch'),
    });

    await h.init();

    assert.deepStrictEqual(h.state.emails.map(e => e.id), ['e1', 'e2']);
    assert.strictEqual(h.state.offlineMode, true);
    assert.strictEqual(h.document.getElementById('offline-banner').textContent,
        'Offline — showing cached mail');
    const statuses = h.calls.filter(c => c.name === 'showStatus').map(c => c.args[0]);
    assert.ok(!statuses.includes('Cannot reach server'),
        'a phone with cached mail must not be told the app is unusable');
});

test('init still dead-ends at "Cannot reach server" with no usable snapshot', async () => {
    const h = makeHarness({ accountsResult: new ApiError('Network error: failed to fetch') });

    await h.init();

    assert.strictEqual(h.state.offlineMode, false, 'there is nothing cached to show');
    const statuses = h.calls.filter(c => c.name === 'showStatus').map(c => c.args[0]);
    assert.ok(statuses.includes('Cannot reach server'));
});

test('an auth failure is NOT swallowed into the offline path', async () => {
    const h = makeHarness({
        storage: { [KEY]: savedBlob() },
        accountsResult: new ApiAuthError('session expired', 401),
    });

    await h.init();

    assert.strictEqual(h.state.offlineMode, false,
        'ApiAuthError means re-authorize, not airplane mode — the taxonomy must survive');
    assert.strictEqual(h.document.getElementById('offline-banner').classList.contains('visible'), false);
    const reported = h.calls.find(c => c.name === 'showError');
    assert.ok(reported, 'the auth failure must still be reported');
    assert.ok(reported.args[1] instanceof ApiAuthError);
});

test('a paint the online restore rejects is cleared, not left on screen', async () => {
    // roborev 517: the pre-fetch paint skips the connected-account check (there
    // is no account list yet). When the fetch then succeeds and the snapshot's
    // account is gone, the online restore rejects it — but the no-connected-
    // account branch never resets the list, so foreign cached mail would sit
    // there fully interactive under a 'No authorized accounts' status.
    const h = makeHarness({
        storage: { [KEY]: savedBlob() },
        accountsResult: [{ id: 'other', email: 'other@example.com', authStatus: 'pending' }],
    });

    await h.init();

    assert.deepStrictEqual(h.state.emails, [],
        'cached mail for an account that is no longer connected must not stay on screen');
    assert.strictEqual(h.state.currentAccount, null);
    const statuses = h.calls.filter(c => c.name === 'showStatus').map(c => c.args[0]);
    assert.ok(statuses.some(s => /No authorized accounts/.test(s)));
});

// ============================================================================
// Pull-to-refresh: the manual escape from a degraded session
// ============================================================================

test('pull-to-refresh retries the whole boot in a degraded session', async () => {
    // roborev 517: this is the ONLY manual recovery when the server is down but
    // the device is online (no 'online' event ever fires), so it needs a pin —
    // a refactor dropping the branch would leave a doomed loadEmails against a
    // session with no mailbox and no splits.
    const h = makeHarness({
        storage: { [KEY]: savedBlob() },
        accountsResult: [{ id: 'acct-1', email: 'me@example.com' }],
        stateOverrides: { offlineMode: true },
    });
    h.document.getElementById('pull-indicator').style.height = '100px';

    h.pullToRefreshRecognizer.end();
    await new Promise(resolve => setImmediate(resolve));

    assert.ok(h.names().includes('loadAccounts'), 'the pull must re-run init()');
    assert.strictEqual(h.state.offlineMode, false, 'a successful retry leaves the degraded session');
    assert.ok(h.names().includes('finishPullRefresh'), 'the indicator must be reset');
});

test('a failed offline pull-to-refresh still resets the indicator', async () => {
    const h = makeHarness({
        storage: { [KEY]: savedBlob() },
        accountsResult: new ApiError('Network error: failed to fetch'),
        stateOverrides: { offlineMode: true },
    });
    h.document.getElementById('pull-indicator').style.height = '100px';

    h.pullToRefreshRecognizer.end();
    await new Promise(resolve => setImmediate(resolve));

    assert.ok(h.names().includes('finishPullRefresh'),
        'a still-unreachable server must not leave the pull indicator stuck open');
    assert.strictEqual(h.state.offlineMode, true, 'the degraded session survives a failed retry');
});

test('pull-to-refresh online still refreshes the list rather than rebooting', () => {
    const h = makeHarness();
    h.document.getElementById('pull-indicator').style.height = '100px';

    h.pullToRefreshRecognizer.end();

    assert.ok(h.names().includes('loadEmails'));
    assert.ok(!h.names().includes('loadAccounts'), 'an online pull must not re-run the boot');
});

// ============================================================================
// roborev 520: degraded-session seams
// ============================================================================

test('handleOnline keeps the cached-mail banner up while the degraded reboot runs', () => {
    // roborev 520: the online event only proves the DEVICE has a network, not
    // that the server answers (captive portal, server outage). Blanking the
    // banner before init() settles leaves dimmed controls with no explanation
    // for the whole loadAccounts timeout.
    const h = makeHarness({
        storage: { [KEY]: savedBlob() },
        accountsResult: new ApiError('Network error: failed to fetch'),
        stateOverrides: { offlineMode: true },
    });
    h.setOfflineMode(true);

    h.handleOnline();

    // Asserted synchronously, before init()'s fetch settles — the transient
    // window is the bug.
    assert.ok(h.document.getElementById('offline-banner').classList.contains('visible'),
        'the cached-mail banner must survive until the reboot actually succeeds');
});

test('a rejected paint also clears restored search and split state', async () => {
    // roborev 520: the offline paint seeds searchQuery, the search input and
    // the open search bar; the rejected-paint cleanup must not leave the
    // foreign query sitting over the no-accounts status.
    const h = makeHarness({
        storage: { [KEY]: savedBlob({ searchQuery: 'from:foreign', splitId: 'work' }) },
        accountsResult: [{ id: 'other', email: 'other@example.com', authStatus: 'pending' }],
    });

    await h.init();

    assert.strictEqual(h.state.searchQuery, '', 'the foreign query must not survive');
    assert.strictEqual(h.state.currentSplit, 'all', 'the foreign split must not survive');
    assert.strictEqual(h.document.getElementById('search-input').value, '');
    assert.strictEqual(
        h.document.getElementById('app-header').classList.contains('searching'), false,
        'the search bar must not stay open over the no-accounts status');
});

test('attachment taps are refused offline instead of opening a doomed tab', () => {
    // roborev 520: cached bodies render live /api/ anchors, and the SW never
    // caches /api/ blobs — the tap can only open a tab onto a failed request.
    const h = makeHarness({ stateOverrides: { offlineMode: true } });
    const handler = h.document.getElementById('detail-attachments').listeners.click;
    assert.ok(handler, 'the delegated attachment click handler must be registered');

    let prevented = false;
    handler({
        target: { closest: sel => (sel.includes('.att-item') ? {} : null) },
        preventDefault: () => { prevented = true; },
    });

    assert.ok(prevented, 'the anchor navigation must be prevented offline');
    assert.ok(h.names().includes('showToast'), 'the refusal must be visible');
    assert.ok(!h.names().includes('downloadAllAttachments'));
});

test('attachment download-all still fires online', () => {
    const h = makeHarness();
    const handler = h.document.getElementById('detail-attachments').listeners.click;

    let prevented = false;
    handler({
        target: { closest: sel => (sel.includes('.att-download-all') ? {} : null) },
        preventDefault: () => { prevented = true; },
    });

    assert.ok(h.names().includes('downloadAllAttachments'),
        'the offline guard must not eat online download-all taps');
    assert.strictEqual(prevented, false);
});

test('an auth failure during a degraded-session reboot drops offline mode', async () => {
    // roborev 521: a degraded session whose handleOnline/pull-to-refresh
    // reboot discovers the session expired must not keep wearing the offline
    // dress — 'showing cached mail' plus disabled controls over an 'Account
    // needs re-authorization' status is the exact mixed signal the ApiAuthError
    // carve-out exists to prevent, and the cold-boot path already leaves
    // controls enabled.
    const h = makeHarness({
        storage: { [KEY]: savedBlob() },
        accountsResult: new ApiAuthError('session expired', 401),
        stateOverrides: { offlineMode: true },
    });
    h.setOfflineMode(true);

    await h.init();

    assert.strictEqual(h.state.offlineMode, false,
        'auth means re-authorize, not airplane mode — even on the reboot path');
    assert.strictEqual(
        h.document.getElementById('offline-banner').classList.contains('visible'), false);
    for (const id of h.OFFLINE_DISABLED_CONTROLS) {
        assert.strictEqual(h.document.getElementById(id).disabled, false,
            `${id} must be re-enabled once the failure is known to be auth, not network`);
    }
});

test('a fully-online persist never parses the previous snapshot blob', () => {
    // roborev 521: persistState runs on visibilitychange/pagehide inside iOS's
    // pre-suspend budget, and the previous blob can now be ~2 MB of bodies. A
    // session that can supply the mailbox and the accounts list itself must
    // not spend that budget parsing a carry-forward source it will not use.
    const h = makeHarness({ storage: { [KEY]: savedBlob() } });
    h.state.accounts = [{ id: 'acct-1', email: 'me@example.com' }];
    h.state.currentAccount = h.state.accounts[0];
    h.state.currentMailbox = { id: 'mb-inbox', role: 'inbox' };
    h.localStorage.reads.length = 0;

    h.persistState();

    assert.deepStrictEqual(h.localStorage.reads, [],
        'no localStorage read may happen when state supplies every carried field');
    assert.deepStrictEqual(h.snapshot().accounts.map(a => a.id), ['acct-1']);
});

test('prefetches past the cache limit never evict opened mail from the cache itself', () => {
    // roborev 522: snapshotBodies ranks opened over prefetched, but it can
    // only rank what survives cache eviction — an opened-blind FIFO at
    // BODY_CACHE_LIMIT (50) lets 3-per-open speculative neighbours push
    // today's actually-read mail out before the snapshot is ever taken.
    // 20 opens with 3 prefetched neighbours each = 80 inserts through 50 slots.
    const h = makeHarness();
    h.state.accounts = [{ id: 'acct-1' }];
    h.state.currentAccount = { id: 'acct-1', email: 'me@example.com' };
    h.state.currentMailbox = { id: 'mb-inbox', role: 'inbox' };
    for (let i = 0; i < 20; i++) {
        h.cacheEmail(body('open' + i), { opened: true });
        for (let j = 0; j < 3; j++) h.cacheEmail(body(`pre${i}_${j}`));
    }

    h.persistState();

    const ids = h.snapshot().bodies.map(b => b.id);
    assert.strictEqual(ids.length, 20);
    assert.ok(ids.every(id => id.startsWith('open')),
        `all 20 opened messages must survive to the snapshot, got ${ids.join(',')}`);
});

test('handleOnline mid-revalidation reroutes to the full boot, not a bare list refresh', async () => {
    // roborev 522: the pre-fetch paint seeds state.accounts from the snapshot
    // while offlineMode is still false and the account fetch is in flight. An
    // online event in that window must not take the bare-refresh path — a
    // mailbox-less loadEmails cannot recover a half-initialized session. The
    // window is entered through a real pending init(), matching how the app
    // reaches it (roborev 524 narrowed the predicate to exactly this).
    const h = makeHarness({
        storage: { [KEY]: savedBlob() },
        accountsResult: [{ id: 'acct-1', email: 'me@example.com' }],
    });

    const boot = h.init();   // paint done synchronously, fetch still pending
    h.handleOnline();

    assert.ok(!h.names().includes('loadEmails'),
        'a bare mailbox-less refresh cannot recover the session');
    await boot;
});

test('an online event during an account switch never reboots into the old account', () => {
    // roborev 524: selectAccount nulls currentMailbox until its cascade
    // resolves. A reboot in that window re-applies the persisted snapshot —
    // written for the PREVIOUS account — silently reverting the user's tap;
    // the bare-refresh path never touches account selection.
    const h = makeHarness({ storage: { [KEY]: savedBlob() } });
    h.state.accounts = [
        { id: 'acct-1', email: 'me@example.com' },
        { id: 'acct-2', email: 'other@example.com' },
    ];
    h.state.currentAccount = h.state.accounts[1];
    h.state.currentMailbox = null;   // the switch's cascade is still in flight

    h.handleOnline();

    assert.ok(!h.names().includes('loadAccounts'),
        'a mid-switch online event must not re-run the boot');
    assert.strictEqual(h.state.currentAccount.id, 'acct-2',
        'the switch must not be reverted to the snapshot account');
});

test('an online event swallowed by an in-flight boot still escapes the degraded session', async () => {
    // roborev 522: init's reentrancy guard eats a reconnect that lands
    // mid-boot. If that boot's fetch (started before the radio came back)
    // then fails and commits to the degraded session, no further online event
    // ever fires — the signal must be remembered and honored with a retry.
    let attempt = 0;
    const h = makeHarness({
        storage: { [KEY]: savedBlob() },
        accountsResult: () => (++attempt === 1
            ? new ApiError('Network error: failed to fetch')
            : [{ id: 'acct-1', email: 'me@example.com' }]),
    });

    const boot = h.init();       // fetch will fail — started before the radio
    h.handleOnline();            // reconnect mid-boot, swallowed by the guard
    await boot;
    await new Promise(resolve => setImmediate(resolve));
    await new Promise(resolve => setImmediate(resolve));

    assert.strictEqual(h.state.offlineMode, false,
        'the reconnect signal must survive the reentrancy guard');
    assert.strictEqual(attempt, 2, 'the boot must have been retried once');
});

test('a swallowed reconnect also rescues the snapshot-less boot', async () => {
    // roborev 523: same race as the degraded-boot rescue, but with no usable
    // snapshot — the boot ends at 'Cannot reach server' with offlineMode
    // still false, and a retry gated on offlineMode alone discards the
    // remembered signal. The device is already online, so no future online
    // event will ever fire; without the retry the session is stuck.
    let attempt = 0;
    const h = makeHarness({
        accountsResult: () => (++attempt === 1
            ? new ApiError('Network error: failed to fetch')
            : [{ id: 'acct-1', email: 'me@example.com', isDefault: true }]),
    });

    const boot = h.init();       // fetch will fail — started before the radio
    h.handleOnline();            // reconnect mid-boot, swallowed by the guard
    await boot;
    await new Promise(resolve => setImmediate(resolve));
    await new Promise(resolve => setImmediate(resolve));

    assert.strictEqual(attempt, 2,
        'the snapshot-less boot must be retried too — it cannot self-recover');
    assert.deepStrictEqual(h.state.accounts.map(a => a.id), ['acct-1']);
});

test('reconnect after an auth-failed painted boot re-runs the full boot', async () => {
    // roborev 525: the ApiAuthError-with-paint exit leaves accounts and
    // currentAccount seeded from the snapshot, offlineMode false, and no
    // mailbox cascade — the one incomplete terminal state the
    // offlineMode/accounts/currentAccount predicates all miss. After the
    // user re-authorizes, recovery signals must reboot, not bare-refresh a
    // session whose bottom nav has no mailboxes to switch to.
    let attempt = 0;
    const h = makeHarness({
        storage: { [KEY]: savedBlob() },
        accountsResult: () => (++attempt === 1
            ? new ApiAuthError('session expired', 401)
            : [{ id: 'acct-1', email: 'me@example.com' }]),
    });
    await h.init();          // paint sticks, auth failure — no degraded session
    h.calls.length = 0;

    h.handleOnline();

    assert.ok(h.names().includes('loadAccounts'),
        'the half-initialized auth aftermath must re-run the boot');
    assert.ok(!h.names().includes('loadEmails'),
        'a bare refresh cannot restore the missing mailbox cascade');
    await new Promise(resolve => setImmediate(resolve));
});

test('pull-to-refresh after an auth-failed painted boot also reboots', async () => {
    let attempt = 0;
    const h = makeHarness({
        storage: { [KEY]: savedBlob() },
        accountsResult: () => (++attempt === 1
            ? new ApiAuthError('session expired', 401)
            : [{ id: 'acct-1', email: 'me@example.com' }]),
    });
    await h.init();
    h.calls.length = 0;
    h.document.getElementById('pull-indicator').style.height = '100px';

    h.pullToRefreshRecognizer.end();
    await new Promise(resolve => setImmediate(resolve));

    assert.ok(h.names().includes('loadAccounts'),
        'the manual escape must work for the auth aftermath too');
    assert.ok(h.names().includes('finishPullRefresh'));
});

test('a quota throw degrades to a bodiless snapshot instead of none', () => {
    // roborev 525: bodies are the declared sacrificial payload, but a
    // setItem that throws despite the serialization cap dropped the WHOLE
    // snapshot — list rows and accounts included — exactly the fields an
    // offline cold start cannot boot without.
    const h = makeHarness();
    h.state.accounts = [{ id: 'acct-1', email: 'me@example.com' }];
    h.state.currentAccount = h.state.accounts[0];
    h.state.currentMailbox = { id: 'mb-inbox', role: 'inbox' };
    h.state.emails = [email('e1')];
    h.cacheEmail(body('e1', 5000), { opened: true });
    const realSet = h.localStorage.setItem.bind(h.localStorage);
    h.localStorage.setItem = (k, v) => {
        if (v.length > 3000) throw new Error('QuotaExceededError');
        realSet(k, v);
    };

    h.persistState();

    const snap = h.snapshot();
    assert.ok(snap, 'the snapshot must survive a quota throw');
    assert.deepStrictEqual(snap.bodies, [], 'bodies are shed on quota');
    assert.deepStrictEqual(snap.emails.map(e => e.id), ['e1'],
        'the list rows a cold start needs must still be written');
    assert.deepStrictEqual(snap.accounts.map(a => a.id), ['acct-1']);
});

test('a successful revalidate swaps in live state and clears the offline banner', async () => {
    const h = makeHarness({
        storage: { [KEY]: savedBlob() },
        accountsResult: [{ id: 'acct-1', email: 'me@example.com' }],
        stateOverrides: { offlineMode: true },
    });

    await h.init();

    assert.strictEqual(h.state.offlineMode, false);
    assert.strictEqual(h.document.getElementById('offline-banner').classList.contains('visible'), false);
    assert.ok(h.names().includes('loadMailboxes'),
        'the online restore must arm the refresh cascade so live state replaces the cache');
});

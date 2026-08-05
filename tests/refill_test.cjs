// Behavioral tests for the refill-vs-optimistic-removal race (kata jg51,
// roborev 470 #1 / 471 #2): the real maybeRefillEmails and
// removeEmailsFromList are extracted from app.js and driven against a
// canned api that returns the server's pre-mutation cached window — the
// exact payload the race serves — so these pin behavior, not source shape.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const APP_JS = fs.readFileSync(path.join(__dirname, '..', 'static', 'app.js'), 'utf8');

function extractFunction(declaration) {
    const start = APP_JS.indexOf(declaration);
    assert.notStrictEqual(start, -1, `${declaration} must exist`);
    const close = APP_JS.indexOf('\n}', start);
    assert.notStrictEqual(close, -1, `${declaration} must close`);
    return APP_JS.slice(start, close + 2);
}

function makeHarness(fetchImpl) {
    const state = {
        emails: [],
        currentMailbox: { id: 'mb-inbox' },
        currentAccount: { id: 'acct' },
        selectedIndex: 0,
    };
    const refillSuppressedIds = new Set();
    const splitListCache = {};
    const noop = () => {};
    const api = async (method, url) => fetchImpl(method, url);
    const code = [
        'let refillInFlight = false;',
        'const REFILL_THRESHOLD = 100;',
        extractFunction('async function maybeRefillEmails('),
        extractFunction('function removeEmailsFromList('),
        'return { maybeRefillEmails, removeEmailsFromList };',
    ].join('\n');
    // eslint-disable-next-line no-new-func
    const fns = new Function(
        'state', 'api', 'refillSuppressedIds', 'splitListCache',
        'splitCacheKey', 'buildEmailListUrl', 'extendThreadGroups',
        'renderEmailList', 'harvestContacts', 'prefetchVisibleEmails',
        'adjustSplitCounts', 'invalidateSplitListCache', 'visibleRows',
        'showStatus',
        code,
    )(
        state, api, refillSuppressedIds, splitListCache,
        () => 'acct:mb-inbox:aristoi:', () => '/api/emails?stub', noop,
        noop, noop, noop, noop, noop, () => state.emails,
        noop,
    );
    return { state, refillSuppressedIds, splitListCache, ...fns };
}

// Harness for the loadEmails wholesale-replace vector (roborev 471 #1):
// during a mutation's round-trip the server answers from its
// pre-invalidate cached window, so the payload must be filtered against
// the suppression set before it reaches state.emails or splitListCache.
function makeLoadHarness(payload, seedCache) {
    const state = {
        emails: [],
        currentMailbox: { id: 'mb-inbox', role: 'inbox' },
        currentAccount: { id: 'acct' },
        selectedIndex: 0,
    };
    const refillSuppressedIds = new Set();
    const splitListCache = seedCache || {};
    const noop = () => {};
    // payload === null models a fetch that never settles, isolating the
    // synchronous eager-repaint path.
    const apiWithMeta = payload === null
        ? () => new Promise(() => {})
        : async () => ({ data: payload, headers: { get: () => null } });
    const els = { emailList: { innerHTML: '' } };
    const code = [
        'let loadEmailsController = null;',
        'let lastRenderedContext = null;',
        extractFunction('async function loadEmails('),
        'return { loadEmails };',
    ].join('\n');
    // eslint-disable-next-line no-new-func
    const fns = new Function(
        'state', 'apiWithMeta', 'refillSuppressedIds', 'splitListCache',
        'splitCacheKey', 'buildEmailListUrl', 'emailListsEqual', 'els',
        'rebuildThreadGroups', 'renderEmailList', 'harvestContacts',
        'prefetchVisibleEmails', 'scheduleStaleRevalidate',
        'staleRevalidateAttempts', 'showStatus', 'loadReminders',
        'renderReminderList',
        code,
    )(
        state, apiWithMeta, refillSuppressedIds, splitListCache,
        () => 'ctx', () => '/api/emails?stub', () => false, els,
        noop, noop, noop, noop, noop, { clear: noop }, noop, noop, noop,
    );
    return { state, refillSuppressedIds, splitListCache, loadEmails: fns.loadEmails };
}

// Harness for the failed-undo revert (roborev 472 #1): performUndo's catch
// removes the re-inserted row because the move back FAILED — a sync to
// server truth with no mutation in flight — so the suppression that
// removeEmailsFromList registers on the way through must not linger.
function makeUndoHarness(fetchImpl) {
    const state = {
        emails: [],
        undoStack: [],
        selectedIndex: 0,
        mailboxes: [{ id: 'mb-inbox', role: 'inbox' }],
        currentMailbox: { id: 'mb-inbox' },
    };
    const refillSuppressedIds = new Set();
    const noop = () => {};
    const els = { undoToast: { classList: { add: noop, remove: noop } } };
    const api = async (...args) => fetchImpl(...args);
    const code = [
        extractFunction('async function performUndo('),
        extractFunction('function removeEmailFromList('),
        extractFunction('function removeEmailsFromList('),
        'return { performUndo };',
    ].join('\n');
    // eslint-disable-next-line no-new-func
    const fns = new Function(
        'state', 'api', 'refillSuppressedIds', 'els', 'showStatus',
        'visibleRowIndexForEmailId', 'extendThreadGroups',
        'invalidateSplitListCache', 'renderEmailList', 'adjustSplitCounts',
        'loadReminders', 'loadSplitCounts', 'maybeRefillEmails',
        'visibleRows', 'splitListCache',
        code,
    )(
        state, api, refillSuppressedIds, els, noop, () => 0, noop,
        noop, noop, noop, noop, noop, noop, () => state.emails, {},
    );
    return { state, refillSuppressedIds, performUndo: fns.performUndo };
}

const row = (id) => ({ id, subject: `Subject ${id}` });

test('jg51: refill cannot resurrect an optimistically removed row', async () => {
    // The canned window is the server's pre-mutation cache: it still
    // contains e2, the row being archived right now.
    const h = makeHarness(async () => [row('e2'), row('e3')]);
    h.state.emails = [row('e1'), row('e2')];

    // removeEmailsFromList registers the suppression and fires the refill
    // itself — the real integration under test.
    h.removeEmailsFromList(e => e.id !== 'e2', 1);
    await new Promise(resolve => setImmediate(resolve));

    assert.deepEqual(
        h.state.emails.map(e => e.id),
        ['e1', 'e3'],
        'the refill must append the genuinely-new row but never the one just removed'
    );
    assert.ok(h.refillSuppressedIds.has('e2'), 'the removal must be registered');
});

test('jg51: releasing the suppression re-admits the row (revert path)', async () => {
    const h = makeHarness(async () => [row('e2'), row('e3')]);
    h.state.emails = [row('e1'), row('e2')];
    h.removeEmailsFromList(e => e.id !== 'e2', 1);
    await new Promise(resolve => setImmediate(resolve));
    assert.deepEqual(h.state.emails.map(e => e.id), ['e1', 'e3']);

    // A revert (or settle) deletes the id — the next refill may serve it.
    h.refillSuppressedIds.delete('e2');
    await h.maybeRefillEmails();

    assert.ok(
        h.state.emails.some(e => e.id === 'e2'),
        'once released, the row must no longer be filtered from refills'
    );
});

test('jg51: loadEmails drops suppressed rows from the list AND the split cache', async () => {
    // The canned payload is the server's pre-invalidate window: e2's
    // archive is still in flight.
    const h = makeLoadHarness([row('e1'), row('e2')]);
    h.refillSuppressedIds.add('e2');

    await h.loadEmails();

    assert.deepEqual(
        h.state.emails.map(e => e.id),
        ['e1'],
        'a wholesale replace must not resurrect the row being archived'
    );
    assert.deepEqual(
        h.splitListCache['ctx'].map(e => e.id),
        ['e1'],
        'the pre-invalidate window must not be written into splitListCache either'
    );
});

test('jg51: removal purges the row from every cached context', () => {
    // The suppression set only covers the in-flight window (settle deletes
    // the id), so a sibling tab's stale cache entry still carrying the row
    // would re-flash it right after the mutation settles (roborev 474).
    // The removal itself must purge all cached contexts.
    const h = makeHarness(async () => []);
    h.state.emails = [row('e1'), row('e2')];
    h.splitListCache['sibling'] = [row('e2'), row('e9')];

    h.removeEmailsFromList(e => e.id !== 'e2', 1);

    assert.deepEqual(
        h.splitListCache['sibling'].map(e => e.id),
        ['e9'],
        'a stale sibling entry must not keep the removed row past settle'
    );
});

test('jg51: eager repaint from a warm splitListCache filters suppressed rows', () => {
    // removeEmailsFromList invalidates only the current context's cache
    // entry, so a sibling tab's cached list can still carry the row whose
    // archive is in flight; switching to that tab mid-round-trip must not
    // flash it back (roborev 473). The fetch never settles here — only the
    // synchronous eager repaint runs.
    const h = makeLoadHarness(null, { ctx: [row('e1'), row('e2')] });
    h.refillSuppressedIds.add('e2');

    h.loadEmails();

    assert.deepEqual(
        h.state.emails.map(e => e.id),
        ['e1'],
        'the warm-cache repaint must not resurrect the row being archived'
    );
});

test('jg51: failed undo must not leave the removed row suppressed', async () => {
    const h = makeUndoHarness(async () => {
        throw new Error('move failed');
    });
    h.state.undoStack.push({
        action: 'archived',
        emailId: 'e2',
        emailData: row('e2'),
        insertIndex: 0,
        reminder: null,
    });

    await h.performUndo();

    assert.equal(h.state.emails.length, 0, 'the failed undo re-removes the row');
    assert.ok(
        !h.refillSuppressedIds.has('e2'),
        'the revert removal syncs to server truth — no mutation is in flight, \
so the id must not stay suppressed for the rest of the session'
    );
});

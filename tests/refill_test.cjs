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
    return { state, refillSuppressedIds, ...fns };
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

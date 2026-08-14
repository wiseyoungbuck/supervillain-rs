// Behavioral tests for explicit email refresh (kata email-refresh).
//
// The server deliberately keeps a warm inbox snapshot for fast account and
// mailbox switches. A user-triggered refresh must opt out of that snapshot;
// these tests run the real URL builders/loaders from both browser bundles and
// assert the observable request contract rather than only checking strings.

const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const DESKTOP = fs.readFileSync(path.join(__dirname, '..', 'static', 'app.js'), 'utf8');
const MOBILE = fs.readFileSync(path.join(__dirname, '..', 'static', 'mobile', 'app.js'), 'utf8');

function extractFunction(source, declaration) {
    const start = source.indexOf(declaration);
    assert.notEqual(start, -1, `${declaration} must exist`);
    const close = source.indexOf('\n}', start);
    assert.notEqual(close, -1, `${declaration} must close`);
    return source.slice(start, close + 2);
}

function desktopState(overrides = {}) {
    return {
        currentAccount: { id: 'acct-1' },
        currentMailbox: { id: 'inbox', role: 'inbox' },
        currentSplit: 'all',
        splits: [],
        starredOnly: false,
        sortOrder: 'date_desc',
        searchTokens: [],
        ...overrides,
    };
}

function loadDesktopUrlBuilder(state) {
    const code = extractFunction(DESKTOP, 'function buildEmailListUrl(')
        + '\nreturn buildEmailListUrl;';
    // eslint-disable-next-line no-new-func
    return new Function('state', 'CACHE_LIMIT', 'getSearchQuery', code)(
        state,
        150,
        () => '',
    );
}

function loadMobileUrlBuilder(state) {
    const code = extractFunction(MOBILE, 'function emailListPath(')
        + '\nreturn emailListPath;';
    // eslint-disable-next-line no-new-func
    return new Function('state', 'PAGE_SIZE', code)(state, 50);
}

function loadDesktopLoader({ initial, fresh, onRequest = null }) {
    const state = desktopState({ emails: initial, selectedIndex: 0 });
    let activeContext = `${state.currentAccount.id}:${state.currentMailbox.id}:all::${state.sortOrder}:`;
    const context = () => activeContext;
    const splitListCache = {};
    const calls = [];
    const statuses = [];
    const splitCountRefreshes = [];
    const statusElement = { textContent: 'Refreshing...' };
    const urlBuilder = loadDesktopUrlBuilder(state);
    const apiWithMeta = async (method, url, body, signal) => {
        calls.push({ method, url });
        if (onRequest) return onRequest({ method, url, body, signal, fresh });
        return {
            data: fresh,
            headers: { get: () => null },
        };
    };

    const code = [
        extractFunction(DESKTOP, 'function emailListsEqual('),
        extractFunction(DESKTOP, 'async function loadEmails('),
        '\nreturn loadEmails;',
    ].join('\n');
    // eslint-disable-next-line no-new-func
    const loadEmails = new Function(
        'state', 'splitListCache', 'loadEmailsController', 'AbortController',
        'splitCacheKey', 'buildEmailListUrl', 'apiWithMeta',
        'refillSuppressedIds', 'els', 'lastRenderedContext',
        'rebuildThreadGroups', 'renderEmailList', 'harvestContacts',
        'prefetchVisibleEmails', 'staleRevalidateAttempts',
        'scheduleStaleRevalidate', 'showStatus', 'loadSplitCounts',
        'retireRefreshStatus', code,
    )(
        state,
        splitListCache,
        null,
        AbortController,
        context,
        urlBuilder,
        apiWithMeta,
        new Set(),
        { emailList: { innerHTML: '' }, statusMessage: statusElement },
        null,
        () => {},
        () => {},
        () => {},
        () => {},
        new Map(),
        () => {},
        (message) => {
            statuses.push(message);
            statusElement.textContent = message;
        },
        () => splitCountRefreshes.push(true),
        () => {
            if (statusElement.textContent === 'Refreshing...') {
                statuses.push('');
                statusElement.textContent = '';
            }
        },
    );
    return {
        state,
        calls,
        statuses,
        splitCountRefreshes,
        loadEmails,
        setContext(value) { activeContext = value; },
        setStatus(value) { statusElement.textContent = value; },
    };
}

test('email-refresh: desktop normal list loads do not opt out of the warm cache', () => {
    const state = desktopState();
    const build = loadDesktopUrlBuilder(state);
    const url = build('inbox');
    assert.doesNotMatch(url, /[?&]refresh=true(?:&|$)/);
});

test('email-refresh: desktop explicit refresh adds the provider-bypass flag', () => {
    const state = desktopState();
    const build = loadDesktopUrlBuilder(state);
    const url = build('inbox', { refresh: true });
    assert.match(url, /[?&]refresh=true(?:&|$)/);
});

test('email-refresh: desktop refresh replaces the visible stale snapshot', async () => {
    const oldEmail = { id: 'old', subject: 'Yesterday' };
    const newEmail = { id: 'new', subject: 'Just arrived' };
    const h = loadDesktopLoader({ initial: [oldEmail], fresh: [newEmail] });

    await h.loadEmails({ refresh: true });

    assert.equal(h.calls.length, 1);
    assert.match(h.calls[0].url, /[?&]refresh=true(?:&|$)/);
    assert.deepEqual(h.state.emails, [newEmail]);
    assert.deepEqual(h.splitCountRefreshes, [true]);
    assert.deepEqual(h.statuses, [''], 'a completed refresh clears Refreshing status');
});

test('email-refresh: a settled refresh cannot clear a later unrelated status', async () => {
    const h = loadDesktopLoader({
        initial: [{ id: 'old', subject: 'Yesterday' }],
        fresh: [{ id: 'new', subject: 'Just arrived' }],
    });
    await h.loadEmails({ refresh: true });
    h.setStatus('Archived');
    h.statuses.push('Archived');
    await h.loadEmails();
    assert.deepEqual(h.statuses, ['', 'Archived']);
});

test('email-refresh: discarded refresh responses retire their status', async () => {
    let h;
    h = loadDesktopLoader({
        initial: [{ id: 'old', subject: 'Yesterday' }],
        fresh: [{ id: 'new', subject: 'Just arrived' }],
        onRequest: ({ fresh }) => {
            h.setContext('acct-1:another-mailbox:all::date_desc:');
            return { data: fresh, headers: { get: () => null } };
        },
    });
    h.statuses.push('Refreshing...');

    await h.loadEmails({ refresh: true });

    assert.equal(h.statuses.at(-1), '');
});

test('email-refresh: an ordinary successor inherits an aborted refresh', async () => {
    let requestNumber = 0;
    let h;
    h = loadDesktopLoader({
        initial: [{ id: 'old', subject: 'Yesterday' }],
        fresh: [{ id: 'new', subject: 'Just arrived' }],
        onRequest: ({ signal, fresh }) => {
            requestNumber++;
            if (requestNumber === 1) {
                return new Promise((resolve, reject) => {
                    signal.addEventListener('abort', () => {
                        const error = new Error('aborted');
                        error.name = 'AbortError';
                        reject(error);
                    });
                });
            }
            return { data: fresh, headers: { get: () => null } };
        },
    });
    h.statuses.push('Refreshing...');

    const first = h.loadEmails({ refresh: true });
    const second = h.loadEmails();
    await Promise.all([first, second]);

    assert.match(h.calls[1].url, /[?&]refresh=true(?:&|$)/);
    assert.equal(h.statuses.at(-1), '', 'the successor must retire the inherited refresh status');
});

test('email-refresh: refresh helper requests fresh data and shows status', () => {
    const calls = [];
    const statuses = [];
    const code = extractFunction(DESKTOP, 'function refreshEmailList(')
        + '\nreturn refreshEmailList;';
    // eslint-disable-next-line no-new-func
    const refresh = new Function('loadEmails', 'showStatus', 'REFRESHING_STATUS', code)(
        (options) => calls.push(options),
        (message) => statuses.push(message),
        'Refreshing...',
    );

    refresh();

    assert.deepEqual(calls, [{ refresh: true }]);
    assert.deepEqual(statuses, ['Refreshing...']);
});

function assertRefreshAction(declaration) {
    const calls = [];
    const name = declaration.match(/function (\w+)/)[1];
    const code = extractFunction(DESKTOP, declaration)
        + '\nreturn ' + name + ';';
    // eslint-disable-next-line no-new-func
    const action = new Function('state', 'refreshEmailList', code)(
        { pendingG: false, view: 'list' },
        () => calls.push(true),
    );
    return { calls, action };
}

test('email-refresh: desktop keyboard R uses the shared refresh action', () => {
    const h = assertRefreshAction('function handleNormalModeKey(');
    h.action({ key: 'R', ctrlKey: false, metaKey: false, altKey: false });
    assert.deepEqual(h.calls, [true]);
});

test('email-refresh: command-palette Refresh uses the shared refresh action', () => {
    const h = assertRefreshAction('function executeCommand(');
    h.action('refresh');
    assert.deepEqual(h.calls, [true]);
});

// Mobile's 50-row requests already bypass the server's 150-row warm slot;
// this pins the explicit intent without claiming it fixes a mobile cache hit.
test('email-refresh: mobile ordinary and pull-to-refresh URLs are distinct', () => {
    const state = {
        currentMailbox: { id: 'inbox', role: 'inbox' },
        currentSplit: 'all',
        splits: [],
        searchQuery: '',
    };
    const build = loadMobileUrlBuilder(state);
    const ordinary = build(0);
    const refreshed = build(0, { refresh: true });

    assert.doesNotMatch(ordinary, /[?&]refresh=true(?:&|$)/);
    assert.match(refreshed, /[?&]refresh=true(?:&|$)/);
});

test('email-refresh: mobile pull-to-refresh is wired to the bypass URL', () => {
    const start = MOBILE.indexOf('const pullToRefreshRecognizer = {');
    assert.notEqual(start, -1);
    const end = MOBILE.indexOf('const rowSwipeRecognizer = {', start);
    assert.notEqual(end, -1);
    const pull = MOBILE.slice(start, end);
    assert.match(pull, /abortListLoad\(\);[\s\S]*loadEmails\(\{ refresh: true \}\)/);
});

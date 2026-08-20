// Behavioral tests for the contact insights sidebar (kata wcsg).
//
// Extracts the REAL functions from static/app.js and exercises their
// runtime output — no copied logic. Same column-0 closing-brace extraction
// convention as invite_chip_test.cjs / routes.rs' js_fn_body.

const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const APP_JS = fs.readFileSync(path.join(__dirname, '..', 'static', 'app.js'), 'utf8');

function extractFunction(declaration) {
    const start = APP_JS.indexOf(declaration);
    assert.notStrictEqual(start, -1, `${declaration} must exist in static/app.js`);
    const close = APP_JS.indexOf('\n}', start);
    assert.notStrictEqual(close, -1, `${declaration} must close with a column-0 brace`);
    return APP_JS.slice(start, close + 2);
}

// Minimal DOM shim for escapeHtml's textContent→innerHTML round-trip —
// same contract as tests/escape_test.cjs's makeDocument.
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

// contactSidebarHtml depends on escapeHtml; bundle both.
function loadSidebarHtml() {
    const code =
        extractFunction('function escapeHtml(') +
        '\n' +
        extractFunction('function contactSidebarHtml(');
    // eslint-disable-next-line no-new-func
    return new Function('document', code + '\nreturn contactSidebarHtml;')(makeDocument());
}

const INSIGHTS = {
    email: 'jane@example.com',
    name: 'Jane Doe',
    messagesFrom: 42,
    messagesTo: 17,
    firstContact: '2024-05-01T10:00:00Z',
    lastContact: '2026-08-15T09:00:00Z',
    recentThreads: [
        { threadId: 't1', emailId: 'm1', subject: 'Quarterly budget', receivedAt: '2026-08-15T09:00:00Z', direction: 'from' },
        { threadId: 't2', emailId: 'm2', subject: 'Offsite plans', receivedAt: '2026-08-10T09:00:00Z', direction: 'to' },
    ],
};

test('wcsg: sidebar renders name, address, counts, first contact, and recent threads', () => {
    const contactSidebarHtml = loadSidebarHtml();
    const html = contactSidebarHtml(INSIGHTS);
    assert.match(html, /Jane Doe/);
    assert.match(html, /jane@example\.com/);
    assert.match(html, /42/, 'received count must render');
    assert.match(html, /17/, 'sent count must render');
    assert.match(html, /First contact/i);
    assert.match(html, /2024/, 'first-contact year must render');
    assert.match(html, /Quarterly budget/);
    assert.match(html, /Offsite plans/);
});

test('wcsg: sidebar escapes hostile names and subjects', () => {
    const contactSidebarHtml = loadSidebarHtml();
    const html = contactSidebarHtml({
        ...INSIGHTS,
        name: '<img src=x onerror=alert(1)>',
        recentThreads: [
            { threadId: 't1', emailId: 'm1', subject: '<script>alert(2)</script>', receivedAt: '2026-08-15T09:00:00Z', direction: 'from' },
        ],
    });
    assert.doesNotMatch(html, /<img src=x/);
    assert.doesNotMatch(html, /<script>alert/);
    assert.match(html, /&lt;script&gt;/);
});

test('wcsg: null insights render a loading state', () => {
    const contactSidebarHtml = loadSidebarHtml();
    const html = contactSidebarHtml(null);
    assert.match(html, /contact-sidebar-loading/);
});

test('wcsg: zero history renders an explicit no-history state, not blank stats', () => {
    const contactSidebarHtml = loadSidebarHtml();
    const html = contactSidebarHtml({
        email: 'new@example.com',
        name: '',
        messagesFrom: 0,
        messagesTo: 0,
        firstContact: null,
        lastContact: null,
        recentThreads: [],
    });
    assert.match(html, /new@example\.com/);
    assert.match(html, /no previous history/i);
});

test('wcsg: loadContactInsights hits the insights endpoint and caches per account+contact', async () => {
    const code = extractFunction('async function loadContactInsights(');
    const calls = [];
    const state = {
        currentAccount: { id: 'acct-1' },
        contactInsights: new Map(),
    };
    const api = async (method, path) => {
        calls.push([method, path]);
        return { ...INSIGHTS };
    };
    // eslint-disable-next-line no-new-func
    const loadContactInsights = new Function(
        'state', 'api', 'console',
        code + '\nreturn loadContactInsights;'
    )(state, api, console);

    const first = await loadContactInsights('Jane@Example.com');
    assert.equal(first.email, 'jane@example.com');
    assert.equal(calls.length, 1);
    const [method, url] = calls[0];
    assert.equal(method, 'GET');
    assert.match(url, /^\/contacts\/insights\?email=Jane%40Example\.com$/,
        'endpoint contract: GET /contacts/insights?email=<urlencoded>');

    // Second call for the same contact (any case) must come from the cache.
    const second = await loadContactInsights('jane@example.com');
    assert.equal(second, first);
    assert.equal(calls.length, 1, 'cache hit must not refetch');
});

test('wcsg: loadContactInsights degrades to null on API failure', async () => {
    const code = extractFunction('async function loadContactInsights(');
    const state = { currentAccount: { id: 'acct-1' }, contactInsights: new Map() };
    const api = async () => { throw new Error('boom'); };
    const quietConsole = { warn: () => {} };
    // eslint-disable-next-line no-new-func
    const loadContactInsights = new Function(
        'state', 'api', 'console',
        code + '\nreturn loadContactInsights;'
    )(state, api, quietConsole);
    assert.equal(await loadContactInsights('jane@example.com'), null);
});

test('wcsg: perf — sidebar render for a 200-message contact under 50ms', () => {
    // The 200-message history arrives pre-aggregated; render cost must stay
    // trivially flat even against an uncapped thread payload. Budget is
    // CI-tolerant (rate_limit.rs discipline).
    const contactSidebarHtml = loadSidebarHtml();
    const big = {
        ...INSIGHTS,
        messagesFrom: 100,
        messagesTo: 100,
        recentThreads: Array.from({ length: 200 }, (_, i) => ({
            threadId: `t${i}`,
            emailId: `m${i}`,
            subject: `Thread subject ${i}`,
            receivedAt: '2026-08-15T09:00:00Z',
            direction: i % 2 ? 'to' : 'from',
        })),
    };
    const start = process.hrtime.bigint();
    const html = contactSidebarHtml(big);
    const elapsedMs = Number(process.hrtime.bigint() - start) / 1e6;
    assert.match(html, /Thread subject 0/);
    assert.ok(elapsedMs < 50, `render took ${elapsedMs.toFixed(2)}ms, budget 50ms`);
});

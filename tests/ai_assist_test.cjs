// Behavioral tests for AI assist (kata 6rhw): summarize-on-detail and
// draft-reply, rendered state from a mocked endpoint — no real API calls.
//
// Extracts the REAL functions from static/app.js (column-0 closing-brace
// convention, same as invite_chip_test.cjs / routes.rs js_fn_body).

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

// Minimal DOM shim for escapeHtml (escape_test.cjs's makeDocument contract).
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

function loadAiSummaryHtml() {
    const code =
        extractFunction('function escapeHtml(') +
        '\n' +
        extractFunction('function aiSummaryHtml(');
    // eslint-disable-next-line no-new-func
    return new Function('document', code + '\nreturn aiSummaryHtml;')(makeDocument());
}

function fakeSummaryEl() {
    return {
        innerHTML: '',
        classes: new Set(['hidden']),
        classList: {
            add(c) { this._el.classes.add(c); },
            remove(c) { this._el.classes.delete(c); },
        },
    };
}

// Wire the classList backreference (plain-object shim).
function makeEls() {
    const el = fakeSummaryEl();
    el.classList._el = el;
    return { aiSummary: el };
}

test('6rhw: aiSummaryHtml renders loading, summary (escaped), and error states', () => {
    const aiSummaryHtml = loadAiSummaryHtml();
    assert.match(aiSummaryHtml({ status: 'loading' }), /ai-summary-loading/);
    const done = aiSummaryHtml({ status: 'done', text: 'Key points <script>x</script>' });
    assert.match(done, /Key points/);
    assert.doesNotMatch(done, /<script>/);
    assert.match(done, /&lt;script&gt;/);
    const err = aiSummaryHtml({ status: 'error', text: 'no key' });
    assert.match(err, /ai-summary-error/);
    assert.match(err, /no key/);
});

test('6rhw: summarizeCurrentEmail posts the email id and renders the summary', async () => {
    const code =
        extractFunction('function escapeHtml(') +
        '\n' + extractFunction('function aiSummaryHtml(') +
        '\n' + extractFunction('async function summarizeCurrentEmail(');
    const calls = [];
    const state = {
        ai: { enabled: true, model: 'claude-sonnet-5' },
        currentEmail: { id: 'e42', from: [{ email: 'jane@example.com' }] },
    };
    const els = makeEls();
    const api = async (method, path, body) => {
        calls.push([method, path, body]);
        return { summary: 'Three <b>key</b> points' };
    };
    // eslint-disable-next-line no-new-func
    const fn = new Function(
        'state', 'els', 'api', 'document', 'console',
        code + '\nreturn summarizeCurrentEmail;'
    )(state, els, api, makeDocument(), console);

    await fn();
    assert.equal(calls.length, 1);
    assert.deepEqual(calls[0], ['POST', '/ai/summarize', { emailId: 'e42' }]);
    assert.ok(!els.aiSummary.classes.has('hidden'), 'summary panel must be visible');
    assert.match(els.aiSummary.innerHTML, /Three &lt;b&gt;key&lt;\/b&gt; points/);
});

test('6rhw: summarizeCurrentEmail renders the error state on API failure', async () => {
    const code =
        extractFunction('function escapeHtml(') +
        '\n' + extractFunction('function aiSummaryHtml(') +
        '\n' + extractFunction('async function summarizeCurrentEmail(');
    const state = {
        ai: { enabled: true },
        currentEmail: { id: 'e42', from: [] },
    };
    const els = makeEls();
    const api = async () => { throw new Error('upstream down'); };
    // eslint-disable-next-line no-new-func
    const fn = new Function(
        'state', 'els', 'api', 'document', 'console',
        code + '\nreturn summarizeCurrentEmail;'
    )(state, els, api, makeDocument(), { warn: () => {} });

    await fn();
    assert.match(els.aiSummary.innerHTML, /ai-summary-error/);
    assert.match(els.aiSummary.innerHTML, /upstream down/);
});

test('6rhw: summarizeCurrentEmail is a no-op when AI assist is disabled', async () => {
    const code =
        extractFunction('function escapeHtml(') +
        '\n' + extractFunction('function aiSummaryHtml(') +
        '\n' + extractFunction('async function summarizeCurrentEmail(');
    const calls = [];
    const state = { ai: { enabled: false }, currentEmail: { id: 'e42' } };
    const els = makeEls();
    const api = async (...a) => { calls.push(a); return {}; };
    // eslint-disable-next-line no-new-func
    const fn = new Function(
        'state', 'els', 'api', 'document', 'console',
        code + '\nreturn summarizeCurrentEmail;'
    )(state, els, api, makeDocument(), console);

    await fn();
    assert.equal(calls.length, 0, 'disabled feature must never call the API');
    assert.ok(els.aiSummary.classes.has('hidden'));
});

test('6rhw: aiDraftReply drafts first, then opens reply compose with the draft body', async () => {
    const code = extractFunction('async function aiDraftReply(');
    const calls = [];
    const replies = [];
    const state = {
        ai: { enabled: true },
        currentEmail: { id: 'e42', from: [{ email: 'jane@example.com' }] },
    };
    const els = { composeBody: { value: '' } };
    const api = async (method, path, body) => {
        calls.push([method, path, body]);
        return { draft: 'Hi Jane,\n\nYes — Friday works.\n' };
    };
    const startReply = (all) => replies.push(all);
    const showStatus = () => {};
    // eslint-disable-next-line no-new-func
    const fn = new Function(
        'state', 'els', 'api', 'startReply', 'showStatus', 'console',
        code + '\nreturn aiDraftReply;'
    )(state, els, api, startReply, showStatus, console);

    await fn();
    assert.deepEqual(calls[0], ['POST', '/ai/draft', { emailId: 'e42' }]);
    assert.deepEqual(replies, [false], 'reply compose opens once, reply-to-sender');
    assert.equal(els.composeBody.value, 'Hi Jane,\n\nYes — Friday works.\n');
});

test('6rhw: aiDraftReply does not open compose when drafting fails', async () => {
    const code = extractFunction('async function aiDraftReply(');
    const replies = [];
    const statuses = [];
    const state = { ai: { enabled: true }, currentEmail: { id: 'e42' } };
    const els = { composeBody: { value: '' } };
    const api = async () => { throw new Error('rate limited'); };
    const startReply = (all) => replies.push(all);
    const showStatus = (msg, type) => statuses.push([msg, type]);
    // eslint-disable-next-line no-new-func
    const fn = new Function(
        'state', 'els', 'api', 'startReply', 'showStatus', 'console',
        code + '\nreturn aiDraftReply;'
    )(state, els, api, startReply, showStatus, { warn: () => {} });

    await fn();
    assert.equal(replies.length, 0, 'a failed draft must not open compose');
    assert.ok(
        statuses.some(([, type]) => type === 'error'),
        'the failure must surface via showStatus(..., "error")'
    );
});

test('6rhw: detail palette offers AI commands only when the feature is enabled', () => {
    const code = extractFunction('function commandsForView(');
    // eslint-disable-next-line no-new-func
    const make = (ai) => new Function(
        'state',
        code + '\nreturn commandsForView;'
    )({
        ai,
        currentEmail: null,
        mailboxes: [],
        splits: [],
        accounts: [],
        undoStack: [],
        currentMailbox: null,
    })('detail').map(c => c.action);

    const withAi = make({ enabled: true });
    assert.ok(withAi.includes('ai-summarize'), 'enabled → ai-summarize offered');
    assert.ok(withAi.includes('ai-draft-reply'), 'enabled → ai-draft-reply offered');

    const withoutAi = make(null);
    assert.ok(!withoutAi.includes('ai-summarize'), 'disabled → hidden, not broken');
    assert.ok(!withoutAi.includes('ai-draft-reply'));
});

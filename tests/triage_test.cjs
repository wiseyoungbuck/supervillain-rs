// Behavioral tests for triage mode — the Get-Me-To-Zero flow (kata 5np4).
//
// Same approach as bulk_ops_test.cjs: extract the REAL functions from
// static/app.js — handleNormalModeKey drives everything, with the real
// emailAction / toggleUnread / removal machinery underneath — and assert
// observable outcomes: which email is focused next, queue/progress state,
// the request contracts, and the failure-stays semantics.
//
// Run:  node --test tests/triage_test.cjs
// Wired into cargo test via tests/triage_test.rs.

const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const APP_JS = fs.readFileSync(
    path.join(__dirname, '..', 'static', 'app.js'),
    'utf8',
);

function extractFunction(src, declaration) {
    const start = src.indexOf(declaration);
    assert.notStrictEqual(start, -1, `${declaration} must exist in app.js`);
    const close = src.indexOf('\n}', start);
    assert.notStrictEqual(close, -1, `${declaration} must close with a column-0 brace`);
    return src.slice(start, close + 2);
}

function extractGetCommands(src) {
    const cfStart = src.indexOf('function commandsForView');
    assert.notStrictEqual(cfStart, -1, 'function commandsForView must exist in app.js');
    const gcStart = src.indexOf('function getCommands', cfStart);
    assert.notStrictEqual(gcStart, -1, 'function getCommands must exist (after commandsForView)');
    const close = src.indexOf('\n}', gcStart);
    return src.slice(cfStart, close + 2);
}

function makeEl() {
    return {
        innerHTML: '',
        textContent: '',
        classList: {
            classes: new Set(['hidden']),
            add(c) { this.classes.add(c); },
            remove(c) { this.classes.delete(c); },
            contains(c) { return this.classes.has(c); },
            toggle(c, on) { if (on) this.classes.add(c); else this.classes.delete(c); },
        },
        addEventListener() {},
        querySelectorAll() { return []; },
    };
}

const email = (id, over) => Object.assign({
    id,
    threadId: '',
    subject: `Subject ${id}`,
    preview: 'p',
    from: [{ name: 'Sender', email: 'sender@example.com' }],
    receivedAt: '2026-08-01T10:00:00Z',
    isUnread: false,
    isFlagged: false,
}, over || {});

function makeState(overrides) {
    return Object.assign(
        {
            view: 'list',
            mode: 'normal',
            selectedIndex: 0,
            currentEmail: null,
            accounts: [],
            splits: [],
            mailboxes: [{ id: 'm-inbox', role: 'inbox', name: 'Inbox' }],
            currentMailbox: { id: 'm-inbox', role: 'inbox', name: 'Inbox' },
            currentSplit: null,
            searchTokens: [],
            undoStack: [],
            emails: [],
            threadGroups: new Map(),
            expandedThreads: new Set(),
            bulkSelected: new Set(),
            bulkAnchorId: null,
            triage: null,
            pendingG: false,
        },
        overrides || {},
    );
}

// The triage environment: real key handler + real triage flow + the real
// action machinery underneath. loadEmailDetail is the observability stub —
// it records which email the flow focused and models the view switch.
function makeTriage(stateOverrides, { apiImpl } = {}) {
    const state = makeState(stateOverrides);
    const apiCalls = [];
    const statuses = [];
    const opened = [];
    const api = (method, path, body) => {
        apiCalls.push({ method, path, body });
        return apiImpl ? apiImpl(method, path, body) : Promise.resolve({});
    };
    const els = {
        emailList: makeEl(),
        bulkBar: makeEl(),
        triageProgress: makeEl(),
        undoToast: makeEl(),
        undoMessage: makeEl(),
        helpOverlay: makeEl(),
    };
    const refillSuppressedIds = new Set();
    const noop = () => {};
    const code = [
        extractFunction(APP_JS, 'function visibleRows('),
        extractFunction(APP_JS, 'function visibleRowIndexForEmailId('),
        extractFunction(APP_JS, 'function getSelectedEmailId('),
        extractFunction(APP_JS, 'function removeEmailFromList('),
        extractFunction(APP_JS, 'function removeEmailsFromList('),
        extractFunction(APP_JS, 'function extendThreadGroups('),
        extractFunction(APP_JS, 'function pushUndo('),
        extractFunction(APP_JS, 'async function emailAction('),
        extractFunction(APP_JS, 'async function toggleUnread('),
        extractFunction(APP_JS, 'function enterTriage('),
        extractFunction(APP_JS, 'function exitTriage('),
        extractFunction(APP_JS, 'function triageOpen('),
        extractFunction(APP_JS, 'function triageAdvance('),
        extractFunction(APP_JS, 'function triageComplete('),
        extractFunction(APP_JS, 'async function triageAction('),
        extractFunction(APP_JS, 'async function triageKeepUnread('),
        extractFunction(APP_JS, 'function renderTriageProgress('),
        extractFunction(APP_JS, 'function handleNormalModeKey('),
        'return { handleNormalModeKey, enterTriage, exitTriage, triageAdvance, triageAction, renderTriageProgress };',
    ].join('\n');
    const deps = {
        state,
        els,
        api,
        refillSuppressedIds,
        splitListCache: {},
        adjustSplitCounts: noop,
        invalidateSplitListCache: noop,
        renderEmailList: noop,
        maybeRefillEmails: noop,
        showStatus: (msg, kind) => { statuses.push({ msg, kind }); },
        loadSplitCounts: noop,
        setTimeout: noop,
        loadEmailDetail: (id) => {
            opened.push(id);
            state.currentEmail = state.emails.find((e) => e.id === id) || { id };
            state.view = 'detail';
        },
        showView: (view) => { state.view = view; },
        moveSelection: (d) => { state.selectedIndex += d; },
        moveToTop: noop,
        moveToBottom: noop,
        openSelected: noop,
        escapeCompose: noop,
        openRemindPicker: noop,
        startReply: noop,
        startCompose: noop,
        startForward: noop,
        toggleUnreadSelected: noop,
        toggleFlagSelected: noop,
        unsubscribeAndArchiveAll: noop,
        performUndo: noop,
        openMovePicker: noop,
        toggleBulkSelect: noop,
        rangeBulkSelect: noop,
        clearBulkSelection: noop,
        rsvpToEvent: noop,
        openSearch: noop,
        refreshEmailList: noop,
        cycleSplit: noop,
        selectAccount: noop,
        openSettings: noop,
        actionSelected: noop,
        goToNextEmail: noop,
    };
    const names = Object.keys(deps);
    // eslint-disable-next-line no-new-func
    const fns = new Function(...names, code)(...names.map((n) => deps[n]));
    return { state, els, apiCalls, statuses, opened, refillSuppressedIds, ...fns };
}

const key = (k, over) => Object.assign({ key: k, ctrlKey: false, metaKey: false, altKey: false, shiftKey: false, preventDefault() {} }, over || {});
const flush = () => new Promise((resolve) => setImmediate(resolve));

const THREE = () => [
    email('e-1', { isUnread: true }),
    email('e-2'),
    email('e-3', { isUnread: true }),
];

test('5np4: T enters triage — unread queue in order, first unread opened, progress shown', () => {
    const t = makeTriage({ emails: THREE() });
    t.handleNormalModeKey(key('T', { shiftKey: true }));
    assert.ok(t.state.triage, 'T from the list must enter triage');
    assert.deepEqual(t.state.triage.queue, ['e-1', 'e-3'], 'the queue is the unread ids, in list order');
    assert.equal(t.state.triage.total, 2);
    assert.deepEqual(t.opened, ['e-1'], 'the first unread must open');
    assert.equal(t.state.view, 'detail');
    assert.equal(t.els.triageProgress.textContent, 'TRIAGE 1/2');
    assert.equal(t.els.triageProgress.classList.contains('hidden'), false);
});

test('5np4: T with nothing unread reports zero and stays put', () => {
    const t = makeTriage({ emails: [email('e-1'), email('e-2')] });
    t.handleNormalModeKey(key('T', { shiftKey: true }));
    assert.equal(t.state.triage, null, 'no unread → no triage');
    assert.equal(t.state.view, 'list');
    assert.ok(
        t.statuses.some((s) => /zero|nothing unread/i.test(s.msg)),
        'the user must be told there is nothing to triage',
    );
});

test('5np4: e archives the current email and advances to the next unread', async () => {
    const t = makeTriage({ emails: THREE() });
    t.handleNormalModeKey(key('T', { shiftKey: true }));
    t.handleNormalModeKey(key('e'));
    await flush();
    assert.ok(
        t.apiCalls.some((c) => c.path === '/emails/e-1/archive'),
        'the archive must POST the existing per-email endpoint',
    );
    assert.ok(!t.state.emails.some((e) => e.id === 'e-1'), 'the archived email leaves the list');
    assert.equal(t.opened[t.opened.length - 1], 'e-3', 'the flow must advance to the NEXT UNREAD, skipping read e-2');
    assert.equal(t.els.triageProgress.textContent, 'TRIAGE 2/2');
});

test('5np4: j skips without acting — email stays, flow advances', () => {
    const t = makeTriage({ emails: THREE() });
    t.handleNormalModeKey(key('T', { shiftKey: true }));
    t.handleNormalModeKey(key('j'));
    assert.equal(t.apiCalls.length, 0, 'skip must not touch the server');
    assert.ok(t.state.emails.some((e) => e.id === 'e-1'), 'the skipped email stays in the list');
    assert.equal(t.opened[t.opened.length - 1], 'e-3');
    assert.equal(t.state.triage.index, 1);
});

test('5np4: u keeps the current email unread and advances', async () => {
    const emails = THREE();
    emails[0].isUnread = false; // opening it in detail marked it read
    const t = makeTriage({ emails });
    t.state.triage = { queue: ['e-1', 'e-3'], index: 0, total: 2 };
    t.state.view = 'detail';
    t.state.currentEmail = emails[0];
    t.handleNormalModeKey(key('u'));
    await flush();
    assert.ok(
        t.apiCalls.some((c) => c.path === '/emails/e-1/mark-unread'),
        'keep-unread must POST mark-unread (the same contract as the single u)',
    );
    assert.equal(t.state.emails[0].isUnread, true, 'the email must be unread again');
    assert.equal(t.opened[t.opened.length - 1], 'e-3', 'and the flow advances');
});

test('5np4: acting on the last queued email lands on the zero state', async () => {
    const t = makeTriage({ emails: [email('e-1', { isUnread: true }), email('e-2')] });
    t.handleNormalModeKey(key('T', { shiftKey: true }));
    t.handleNormalModeKey(key('e'));
    await flush();
    assert.equal(t.state.triage, null, 'the flow must end after the last email');
    assert.equal(t.state.view, 'list', 'and return to the list');
    assert.equal(t.els.triageProgress.classList.contains('hidden'), true, 'progress hides');
    assert.ok(
        t.statuses.some((s) => /zero|complete/i.test(s.msg)),
        'the zero state must be announced',
    );
});

test('5np4: a failed action reverts and STAYS on the failed email', async () => {
    const t = makeTriage(
        { emails: THREE() },
        { apiImpl: (m, path) => (path.endsWith('/archive') ? Promise.reject(new Error('boom')) : Promise.resolve({})) },
    );
    t.handleNormalModeKey(key('T', { shiftKey: true }));
    t.handleNormalModeKey(key('e'));
    await flush();
    assert.ok(
        t.state.emails.some((e) => e.id === 'e-1'),
        'the failed archive must revert (emailAction catch) — the email is back',
    );
    assert.ok(t.state.triage, 'triage must stay active');
    assert.equal(t.state.triage.index, 0, 'the flow must NOT advance past a lost action');
    assert.deepEqual(t.opened, ['e-1'], 'no new email may open — we stay on the failed one');
    assert.equal(t.els.triageProgress.textContent, 'TRIAGE 1/2');
});

test('5np4: Escape exits triage back to the list without acting', () => {
    const t = makeTriage({ emails: THREE() });
    t.handleNormalModeKey(key('T', { shiftKey: true }));
    t.handleNormalModeKey(key('Escape'));
    assert.equal(t.state.triage, null);
    assert.equal(t.state.view, 'list');
    assert.equal(t.state.emails.length, 3, 'exit must not touch any email');
    assert.equal(t.apiCalls.length, 0);
    assert.equal(t.els.triageProgress.classList.contains('hidden'), true);
});

test('5np4: the queue tolerates ids that vanished outside the flow', () => {
    const t = makeTriage({
        emails: [
            email('e-1', { isUnread: true }),
            email('e-3', { isUnread: true }),
            email('e-5', { isUnread: true }),
        ],
    });
    t.handleNormalModeKey(key('T', { shiftKey: true }));
    // e-3 disappears behind triage's back (another tab, a refill replace).
    t.state.emails = t.state.emails.filter((e) => e.id !== 'e-3');
    t.handleNormalModeKey(key('j'));
    assert.equal(t.opened[t.opened.length - 1], 'e-5', 'advance must skip over a vanished id');
});

test('5np4: the palette offers Triage Mode only when something is unread', () => {
    const region = extractGetCommands(APP_JS);
    const load = (state) =>
        // eslint-disable-next-line no-new-func
        new Function('state', 'visibleRows', 'bulkIds', region + '\nreturn getCommands;')(
            state, () => [], () => [],
        )().map((c) => c.action);
    const withUnread = load(makeState({ emails: [email('e-1', { isUnread: true })] }));
    assert.ok(withUnread.includes('triage'), 'unread present → offer Triage Mode');
    const allRead = load(makeState({ emails: [email('e-1')] }));
    assert.ok(!allRead.includes('triage'), 'nothing unread → the flow would be a no-op; do not offer it');
});

test('5np4: the palette command enters the real flow', () => {
    const t = makeTriage({ emails: THREE() });
    const code = extractFunction(APP_JS, 'function executeCommand(') + '\nreturn executeCommand;';
    // eslint-disable-next-line no-new-func
    const executeCommand = new Function('state', 'enterTriage', code)(t.state, t.enterTriage);
    executeCommand('triage');
    assert.ok(t.state.triage, "the palette's triage command must enter the flow");
    assert.deepEqual(t.opened, ['e-1']);
});

test('5np4 perf: 100 triage keystrokes over a 1,000-email list stay under 200ms', () => {
    // Budget per the kata plan's perf table (~5x local headroom for CI).
    // 100 skip keystrokes through the real handler over 1,000 unread emails;
    // each advance walks the queue and re-resolves the visible row.
    const emails = [];
    for (let i = 0; i < 1000; i++) emails.push(email(`e-${i}`, { isUnread: true }));
    const t = makeTriage({ emails });
    t.handleNormalModeKey(key('T', { shiftKey: true }));
    const start = process.hrtime.bigint();
    for (let i = 0; i < 100; i++) t.handleNormalModeKey(key('j'));
    const elapsedMs = Number(process.hrtime.bigint() - start) / 1e6;
    assert.equal(t.state.triage.index, 100, 'all 100 skips must land');
    assert.equal(t.opened.length, 101, 'each skip must focus the next email');
    assert.ok(elapsedMs < 200, `100 triage keystrokes took ${elapsedMs.toFixed(1)}ms (budget 200ms total)`);
});

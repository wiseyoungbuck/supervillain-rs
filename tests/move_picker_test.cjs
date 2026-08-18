// Behavioral tests for the Move to Folder / Apply Label picker (kata e993,
// Superhuman 'v'/'l').
//
// Same approach as tests/palette_test.cjs: extract the REAL functions from
// static/app.js, stand up stub `state` / `els` / `document`, and assert
// observable outcomes — the rendered picker markup, the state changes, and the
// POST /emails/{id}/move request contract — never "the right function was
// called".
//
// Run:  node --test tests/move_picker_test.cjs
// Wired into cargo test via tests/move_picker_test.rs (mirrors palette_test).

const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const APP_JS = fs.readFileSync(
    path.join(__dirname, '..', 'static', 'app.js'),
    'utf8',
);

// Extract one real column-0 function body from app.js (the js_fn_body
// convention shared with the Rust contract tests and palette_test.cjs).
function extractFunction(src, declaration) {
    const start = src.indexOf(declaration);
    assert.notStrictEqual(start, -1, `${declaration} must exist in app.js`);
    const close = src.indexOf('\n}', start);
    assert.notStrictEqual(close, -1, `${declaration} must close with a column-0 brace`);
    return src.slice(start, close + 2);
}

// The commandsForView..getCommands contiguous region (palette string pin).
function extractGetCommands(src) {
    const cfStart = src.indexOf('function commandsForView');
    assert.notStrictEqual(cfStart, -1, 'function commandsForView must exist in app.js');
    const gcStart = src.indexOf('function getCommands', cfStart);
    assert.notStrictEqual(gcStart, -1, 'function getCommands must exist (after commandsForView)');
    const close = src.indexOf('\n}', gcStart);
    return src.slice(cfStart, close + 2);
}

// Browser-faithful escape shim (same as palette_test.cjs).
function makeEscapeDocument() {
    return {
        createElement() {
            let text = '';
            return {
                set textContent(value) { text = String(value); },
                get innerHTML() {
                    return text
                        .replace(/&/g, '&amp;')
                        .replace(/</g, '&lt;')
                        .replace(/>/g, '&gt;');
                },
            };
        },
    };
}

// A fake element good enough for the picker: innerHTML records what the real
// code assigned; querySelectorAll returns [] so listener-attach loops no-op.
function makeEl() {
    return {
        innerHTML: '',
        value: '',
        classList: {
            classes: new Set(['hidden']),
            add(c) { this.classes.add(c); },
            remove(c) { this.classes.delete(c); },
            contains(c) { return this.classes.has(c); },
        },
        addEventListener() {},
        querySelectorAll() { return []; },
        focus() {},
    };
}

// Stub state with every field the picker reads.
function makeState(overrides) {
    return Object.assign(
        {
            view: 'list',
            selectedIndex: 0,
            currentEmail: null,
            accounts: [],
            splits: [],
            mailboxes: [],
            currentMailbox: null,
            currentAccount: null,
            undoStack: [],
            emails: [],
            movePickerOpen: false,
            moveEmailId: null,
            movePickerIndex: 0,
            bulkSelected: new Set(),
            bulkAnchorId: null,
        },
        overrides || {},
    );
}

const MAILBOXES = [
    { id: 'm-inbox', role: 'inbox', name: 'Inbox' },
    { id: 'm-archive', role: 'archive', name: 'Archive' },
    { id: 'm-work', role: null, name: 'Work' },
    { id: 'm-recruiters', role: null, name: 'Recruiters' },
];

// Build the full picker environment: real picker functions + real escape
// helpers + real removal/undo machinery, leaf deps injected. Returns the
// bound functions plus the recorders the tests assert against.
function makePicker(stateOverrides, { apiImpl } = {}) {
    const state = makeState({
        mailboxes: MAILBOXES,
        currentMailbox: MAILBOXES[0],
        ...stateOverrides,
    });
    const apiCalls = [];
    const api = (method, path, body) => {
        apiCalls.push({ method, path, body });
        return apiImpl ? apiImpl(method, path, body) : Promise.resolve({});
    };
    const moveModal = makeEl();
    const filterEl = makeEl();
    const listEl = makeEl();
    const els = {
        moveModal,
        undoToast: makeEl(),
        undoMessage: makeEl(),
    };
    const escDoc = makeEscapeDocument();
    const document = {
        createElement: (...a) => escDoc.createElement(...a),
        getElementById(id) {
            if (id === 'move-filter') return filterEl;
            if (id === 'move-picker-list') return listEl;
            return makeEl();
        },
    };
    const refillSuppressedIds = new Set();
    const noop = () => {};
    const code = [
        extractFunction(APP_JS, 'function escapeHtml('),
        extractFunction(APP_JS, 'function escapeAttr('),
        extractFunction(APP_JS, 'function bulkIds('),
        extractFunction(APP_JS, 'function movePickerMailboxes('),
        extractFunction(APP_JS, 'function renderMovePickerList('),
        extractFunction(APP_JS, 'function openMovePicker('),
        extractFunction(APP_JS, 'function handleMovePickerKey('),
        extractFunction(APP_JS, 'function closeMovePicker('),
        extractFunction(APP_JS, 'function confirmMovePicker('),
        extractFunction(APP_JS, 'async function moveEmailTo('),
        extractFunction(APP_JS, 'function pushUndo('),
        extractFunction(APP_JS, 'async function performUndo('),
        extractFunction(APP_JS, 'function removeEmailFromList('),
        extractFunction(APP_JS, 'function removeEmailsFromList('),
        extractFunction(APP_JS, 'function getSelectedEmailId('),
        'return { openMovePicker, handleMovePickerKey, closeMovePicker, moveEmailTo, performUndo, renderMovePickerList };',
    ].join('\n');
    const deps = {
        state,
        els,
        document,
        api,
        refillSuppressedIds,
        splitListCache: {},
        visibleRows: () => state.emails.map((e) => ({ emailId: e.id })),
        adjustSplitCounts: noop,
        invalidateSplitListCache: noop,
        renderEmailList: noop,
        maybeRefillEmails: noop,
        showStatus: noop,
        goToNextEmail: noop,
        loadSplitCounts: noop,
        loadReminders: noop,
        setMode: noop,
        setTimeout: noop, // shadow the global: no 5s undo-toast timer in tests
        visibleRowIndexForEmailId: () => 0,
        extendThreadGroups: noop,
    };
    const names = Object.keys(deps);
    // eslint-disable-next-line no-new-func
    const fns = new Function(...names, code)(...names.map((n) => deps[n]));
    return { state, els, filterEl, listEl, apiCalls, refillSuppressedIds, ...fns };
}

test('e993: opening the picker lists every mailbox except the current one', () => {
    const p = makePicker({ emails: [{ id: 'e-1' }] });
    p.openMovePicker('e-1');
    assert.equal(p.state.movePickerOpen, true);
    assert.equal(p.els.moveModal.classList.contains('hidden'), false, 'the modal must show');
    assert.match(p.els.moveModal.innerHTML, /Move to folder/, 'non-Gmail accounts file into folders');
    assert.ok(p.listEl.innerHTML.includes('Archive'), 'other mailboxes must be listed');
    assert.ok(p.listEl.innerHTML.includes('Work'));
    assert.ok(
        !p.listEl.innerHTML.includes('Inbox'),
        'the current mailbox must be excluded — moving an email to where it already is is a no-op',
    );
});

test('e993: Gmail accounts see label wording (provider semantics mapping)', () => {
    const p = makePicker({
        emails: [{ id: 'e-1' }],
        currentAccount: { id: 'g', provider: 'gmail' },
    });
    p.openMovePicker('e-1');
    assert.match(p.els.moveModal.innerHTML, /Move to label/, "Gmail's folders are labels");
});

test('e993: the picker does not open without a selected email or mailboxes', () => {
    const none = makePicker({ emails: [] });
    none.openMovePicker(undefined);
    assert.equal(none.state.movePickerOpen, false, 'no selection → no picker');

    const empty = makePicker({ emails: [{ id: 'e-1' }], mailboxes: [] });
    empty.openMovePicker('e-1');
    assert.equal(empty.state.movePickerOpen, false, 'no mailboxes → nothing to move to');
});

test('e993: type-to-filter narrows the list case-insensitively', () => {
    const p = makePicker({ emails: [{ id: 'e-1' }] });
    p.openMovePicker('e-1');
    p.filterEl.value = 'reCRUIT';
    p.renderMovePickerList();
    assert.ok(p.listEl.innerHTML.includes('Recruiters'));
    assert.ok(!p.listEl.innerHTML.includes('Archive'), 'non-matching mailboxes must drop out');
});

test('e993: mailbox names render as text, ids escape into attributes (fhtz discipline)', () => {
    const p = makePicker({
        emails: [{ id: 'e-1' }],
        mailboxes: [
            { id: 'm-inbox', role: 'inbox', name: 'Inbox' },
            { id: 'm-evil" onmouseover="x', role: null, name: '<img src=x onerror="pwn">' },
        ],
    });
    p.openMovePicker('e-1');
    assert.ok(!p.listEl.innerHTML.includes('<img'), 'a markup-named mailbox must not render live');
    assert.ok(p.listEl.innerHTML.includes('&lt;img'), 'the name must render as escaped text');
    assert.ok(
        !p.listEl.innerHTML.includes('id="m-evil"'), // raw quote would break out of the attribute
        'quote-bearing ids must be attribute-escaped',
    );
});

test('e993: ArrowDown/ArrowUp move the selection and clamp at the ends', () => {
    const p = makePicker({ emails: [{ id: 'e-1' }] });
    p.openMovePicker('e-1');
    // 3 candidates (inbox excluded): Archive, Work, Recruiters
    for (let i = 0; i < 10; i++) p.handleMovePickerKey({ key: 'ArrowDown', preventDefault() {} });
    assert.equal(p.state.movePickerIndex, 2, 'selection must clamp at the last candidate');
    p.handleMovePickerKey({ key: 'ArrowUp', preventDefault() {} });
    assert.equal(p.state.movePickerIndex, 1);
});

test('e993: Enter moves the selected email — POST contract + optimistic removal', () => {
    const p = makePicker({ emails: [{ id: 'e-1' }, { id: 'e-2' }] });
    p.openMovePicker('e-1');
    p.handleMovePickerKey({ key: 'ArrowDown', preventDefault() {} }); // Archive → Work
    p.handleMovePickerKey({ key: 'Enter', preventDefault() {} });
    assert.equal(p.state.movePickerOpen, false, 'confirming must close the picker');
    assert.equal(p.els.moveModal.classList.contains('hidden'), true);
    assert.deepEqual(
        p.state.emails.map((e) => e.id),
        ['e-2'],
        'the moved email must leave the list immediately (optimistic)',
    );
    assert.deepEqual(p.apiCalls, [
        { method: 'POST', path: '/emails/e-1/move', body: { mailbox_id: 'm-work' } },
    ]);
});

test('e993: Escape cancels without touching the list or the server', () => {
    const p = makePicker({ emails: [{ id: 'e-1' }] });
    p.openMovePicker('e-1');
    p.handleMovePickerKey({ key: 'Escape', preventDefault() {} });
    assert.equal(p.state.movePickerOpen, false);
    assert.deepEqual(p.state.emails.map((e) => e.id), ['e-1'], 'cancel must not move anything');
    assert.equal(p.apiCalls.length, 0);
});

test('e993: a failed move reverts the removal and releases refill suppression', async () => {
    const p = makePicker(
        { emails: [{ id: 'e-1' }, { id: 'e-2' }] },
        { apiImpl: () => Promise.reject(new Error('boom')) },
    );
    await p.moveEmailTo('e-1', { id: 'm-work', name: 'Work' });
    assert.deepEqual(
        p.state.emails.map((e) => e.id),
        ['e-1', 'e-2'],
        'a failed move must restore the email at its original position',
    );
    assert.equal(p.state.undoStack.length, 0, 'the stale undo entry must pop on failure');
    assert.equal(p.refillSuppressedIds.size, 0, 'suppression must release on revert');
});

test('e993: a settled move releases refill suppression', async () => {
    const p = makePicker({ emails: [{ id: 'e-1' }] });
    await p.moveEmailTo('e-1', { id: 'm-work', name: 'Work' });
    assert.equal(p.refillSuppressedIds.size, 0, 'suppression must release once the POST settles');
});

test('e993: undo after a move restores to the SOURCE mailbox, not the inbox', async () => {
    const p = makePicker({
        emails: [{ id: 'e-1' }],
        currentMailbox: MAILBOXES[2], // viewing Work
    });
    await p.moveEmailTo('e-1', { id: 'm-recruiters', name: 'Recruiters' });
    assert.equal(p.state.undoStack.length, 1, 'a move must be undoable');
    await p.performUndo();
    assert.deepEqual(p.state.emails.map((e) => e.id), ['e-1'], 'undo must re-insert the email');
    assert.equal(p.apiCalls.length, 2);
    assert.deepEqual(
        p.apiCalls[1],
        { method: 'POST', path: '/emails/e-1/move', body: { mailbox_id: 'm-work' } },
        'undo must move the email back to where it came from (Work), not to the inbox',
    );
});

test('e993: archive undo still restores to the inbox (no sourceMailboxId)', async () => {
    // emailAction pushes undo items without a source mailbox; the fallback
    // must stay the inbox so the existing archive/trash undo is unchanged.
    const p = makePicker({ emails: [] });
    p.state.undoStack.push({
        action: 'archived', emailId: 'e-9', emailData: { id: 'e-9' }, insertIndex: 0,
    });
    await p.performUndo();
    assert.deepEqual(p.apiCalls, [
        { method: 'POST', path: '/emails/e-9/move', body: { mailbox_id: 'm-inbox' } },
    ]);
});

test('e993: the palette offers Move to Folder in list (with selection) and detail', () => {
    const region = extractGetCommands(APP_JS);
    const load = (state, visibleRows) =>
        // eslint-disable-next-line no-new-func
        new Function('state', 'visibleRows', 'bulkIds', region + '\nreturn getCommands;')(
            state, visibleRows, () => [],
        )().map((c) => c.action);
    const withSel = load(makeState({ view: 'list', mailboxes: MAILBOXES }), () => [{ emailId: 'e-1' }]);
    assert.ok(withSel.includes('move-to'), 'list with a selection must offer Move to Folder');
    const noSel = load(makeState({ view: 'list', mailboxes: MAILBOXES }), () => []);
    assert.ok(!noSel.includes('move-to'), 'no selection → nothing to move');
    const detail = load(
        makeState({ view: 'detail', currentEmail: { id: 'e-1' }, mailboxes: MAILBOXES }),
        () => [],
    );
    assert.ok(detail.includes('move-to'), 'detail must offer Move to Folder for the open email');
    const noBoxes = load(makeState({ view: 'list', mailboxes: [] }), () => [{ emailId: 'e-1' }]);
    assert.ok(!noBoxes.includes('move-to'), 'no mailboxes → the picker would be empty; do not offer it');
});

test('e993: the palette command opens the real picker', () => {
    const p = makePicker({ emails: [{ id: 'e-1' }], view: 'list', selectedIndex: 0 });
    const code = [
        extractFunction(APP_JS, 'function executeCommand('),
        'return executeCommand;',
    ].join('\n');
    // eslint-disable-next-line no-new-func
    const executeCommand = new Function(
        'state', 'openMovePicker', 'getSelectedEmailId',
        code,
    )(p.state, p.openMovePicker, () => 'e-1');
    executeCommand('move-to');
    assert.equal(p.state.movePickerOpen, true, "the palette's move-to must open the picker");
});

test('e993 perf: rendering the picker over 500 mailboxes stays under 100ms', () => {
    // Budget per the kata plan's perf table (~5x local headroom for CI).
    // Synthetic mailboxes, no network; filter + innerHTML string build is
    // the cost being bounded.
    const mailboxes = [{ id: 'm-cur', role: 'inbox', name: 'Inbox' }];
    for (let i = 0; i < 500; i++) {
        mailboxes.push({ id: `m-${i}`, role: null, name: `Folder ${i}` });
    }
    const p = makePicker({ emails: [{ id: 'e-1' }], mailboxes, currentMailbox: mailboxes[0] });
    p.openMovePicker('e-1');
    const start = process.hrtime.bigint();
    p.renderMovePickerList();
    const elapsedMs = Number(process.hrtime.bigint() - start) / 1e6;
    assert.ok(p.listEl.innerHTML.includes('Folder 499'), 'all 500 mailboxes must render');
    assert.ok(elapsedMs < 100, `picker render over 500 mailboxes took ${elapsedMs.toFixed(1)}ms (budget 100ms)`);
});

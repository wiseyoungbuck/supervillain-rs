// Behavioral tests for bulk selection (kata pakx): x/Shift+X selection state
// after real keystroke sequences through the real handleNormalModeKey, batch
// request contracts over the existing per-email endpoints, one undo entry
// restoring a whole batch, partial-failure revert, selection lifecycle across
// context switches, palette gating, rendered selection markup, and the
// 1,000-email/500-selected render budget.
//
// Same approach as palette_test.cjs / move_picker_test.cjs: extract the REAL
// functions from static/app.js, inject leaf dependencies as recording stubs,
// assert observable outcomes.
//
// Run:  node --test tests/bulk_ops_test.cjs
// Wired into cargo test via tests/bulk_ops_test.rs.

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

function makeEl() {
    return {
        innerHTML: '',
        textContent: '',
        value: '',
        classList: {
            classes: new Set(['hidden']),
            add(c) { this.classes.add(c); },
            remove(c) { this.classes.delete(c); },
            contains(c) { return this.classes.has(c); },
            toggle(c, on) { if (on) this.classes.add(c); else this.classes.delete(c); },
        },
        addEventListener() {},
        querySelectorAll() { return []; },
        focus() {},
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
            currentAccount: null,
            currentSplit: null,
            splitCounts: {},
            searchTokens: [],
            undoStack: [],
            emails: [],
            threadGroups: new Map(),
            expandedThreads: new Set(),
            bulkSelected: new Set(),
            bulkAnchorId: null,
            movePickerOpen: false,
            moveEmailId: null,
            movePickerIndex: 0,
            pendingG: false,
        },
        overrides || {},
    );
}

// The full bulk environment: real selection + batch + undo + picker machinery
// with the real visibleRows/getSelectedEmailId/removeEmailsFromList, and real
// handleNormalModeKey so keystroke sequences drive everything end to end.
function makeBulk(stateOverrides, { apiImpl } = {}) {
    const state = makeState(stateOverrides);
    const apiCalls = [];
    const api = (method, path, body) => {
        apiCalls.push({ method, path, body });
        return apiImpl ? apiImpl(method, path, body) : Promise.resolve({});
    };
    const els = {
        emailList: makeEl(),
        bulkBar: makeEl(),
        undoToast: makeEl(),
        undoMessage: makeEl(),
        moveModal: makeEl(),
        helpOverlay: makeEl(),
    };
    const filterEl = makeEl();
    const listEl = makeEl();
    const escDoc = makeEscapeDocument();
    const document = {
        createElement: (...a) => escDoc.createElement(...a),
        getElementById(id) {
            if (id === 'move-filter') return filterEl;
            if (id === 'move-picker-list') return listEl;
            return makeEl();
        },
        activeElement: null,
    };
    const refillSuppressedIds = new Set();
    const noop = () => {};
    const code = [
        extractFunction(APP_JS, 'function escapeHtml('),
        extractFunction(APP_JS, 'function escapeAttr('),
        extractFunction(APP_JS, 'function visibleRows('),
        extractFunction(APP_JS, 'function visibleRowIndexForEmailId('),
        extractFunction(APP_JS, 'function getSelectedEmailId('),
        extractFunction(APP_JS, 'function removeEmailFromList('),
        extractFunction(APP_JS, 'function removeEmailsFromList('),
        extractFunction(APP_JS, 'function extendThreadGroups('),
        extractFunction(APP_JS, 'function bulkIds('),
        extractFunction(APP_JS, 'function toggleBulkSelect('),
        extractFunction(APP_JS, 'function rangeBulkSelect('),
        extractFunction(APP_JS, 'function selectAllVisible('),
        extractFunction(APP_JS, 'function clearBulkSelection('),
        extractFunction(APP_JS, 'function renderBulkBar('),
        extractFunction(APP_JS, 'function pushBulkUndo('),
        extractFunction(APP_JS, 'async function bulkRemoveAndSend('),
        extractFunction(APP_JS, 'function bulkEmailAction('),
        extractFunction(APP_JS, 'function bulkMove('),
        extractFunction(APP_JS, 'async function bulkToggleUnread('),
        extractFunction(APP_JS, 'async function bulkToggleFlag('),
        extractFunction(APP_JS, 'function actionSelected('),
        extractFunction(APP_JS, 'function toggleUnreadSelected('),
        extractFunction(APP_JS, 'function toggleFlagSelected('),
        extractFunction(APP_JS, 'function pushUndo('),
        extractFunction(APP_JS, 'async function performUndo('),
        extractFunction(APP_JS, 'function movePickerMailboxes('),
        extractFunction(APP_JS, 'function renderMovePickerList('),
        extractFunction(APP_JS, 'function openMovePicker('),
        extractFunction(APP_JS, 'function closeMovePicker('),
        extractFunction(APP_JS, 'function confirmMovePicker('),
        extractFunction(APP_JS, 'async function moveEmailTo('),
        extractFunction(APP_JS, 'function handleNormalModeKey('),
        'return { handleNormalModeKey, toggleBulkSelect, rangeBulkSelect, selectAllVisible, clearBulkSelection, bulkIds, bulkEmailAction, bulkToggleUnread, bulkToggleFlag, bulkMove, performUndo, actionSelected, toggleUnreadSelected, toggleFlagSelected, openMovePicker, confirmMovePicker, renderBulkBar };',
    ].join('\n');
    const deps = {
        state,
        els,
        document,
        api,
        refillSuppressedIds,
        splitListCache: {},
        adjustSplitCounts: noop,
        invalidateSplitListCache: noop,
        renderEmailList: noop,
        maybeRefillEmails: noop,
        showStatus: noop,
        goToNextEmail: noop,
        loadSplitCounts: noop,
        loadReminders: noop,
        loadEmails: noop,
        setMode: noop,
        setTimeout: noop,
        moveSelection: (d) => { state.selectedIndex += d; },
        moveToTop: noop,
        moveToBottom: noop,
        openSelected: noop,
        showView: noop,
        escapeCompose: noop,
        openRemindPicker: noop,
        startReply: noop,
        startCompose: noop,
        startForward: noop,
        toggleUnread: noop,
        toggleFlag: noop,
        unsubscribeAndArchiveAll: noop,
        rsvpToEvent: noop,
        openSearch: noop,
        refreshEmailList: noop,
        cycleSplit: noop,
        selectAccount: noop,
        openSettings: noop,
        emailAction: noop,
    };
    const names = Object.keys(deps);
    // eslint-disable-next-line no-new-func
    const fns = new Function(...names, code)(...names.map((n) => deps[n]));
    return { state, els, filterEl, listEl, apiCalls, refillSuppressedIds, ...fns };
}

const key = (k, over) => Object.assign({ key: k, ctrlKey: false, metaKey: false, altKey: false, shiftKey: false, preventDefault() {} }, over || {});

test('pakx: x toggles selection by email id through the real key handler', () => {
    const b = makeBulk({ emails: [email('e-1'), email('e-2')] });
    b.handleNormalModeKey(key('x'));
    assert.deepEqual([...b.state.bulkSelected], ['e-1'], 'x must select the current row');
    b.handleNormalModeKey(key('x'));
    assert.equal(b.state.bulkSelected.size, 0, 'x again must deselect');
});

test('pakx: x then j x builds a two-email selection; Shift+X selects the range', () => {
    const b = makeBulk({ emails: ['e-1', 'e-2', 'e-3', 'e-4', 'e-5'].map((id) => email(id)) });
    b.handleNormalModeKey(key('x'));
    b.handleNormalModeKey(key('j'));
    b.handleNormalModeKey(key('x'));
    assert.deepEqual([...b.state.bulkSelected].sort(), ['e-1', 'e-2']);

    // Range: anchor is the last-toggled row; jump ahead and Shift+X fills in.
    b.handleNormalModeKey(key('j'));
    b.handleNormalModeKey(key('j'));
    b.handleNormalModeKey(key('X', { shiftKey: true }));
    assert.deepEqual(
        [...b.state.bulkSelected].sort(),
        ['e-1', 'e-2', 'e-3', 'e-4'],
        'Shift+X must select everything between the anchor and the cursor',
    );
});

test('pakx: Escape clears the selection in list view', () => {
    const b = makeBulk({ emails: [email('e-1'), email('e-2')] });
    b.handleNormalModeKey(key('x'));
    assert.equal(b.state.bulkSelected.size, 1);
    b.handleNormalModeKey(key('Escape'));
    assert.equal(b.state.bulkSelected.size, 0, 'Escape must clear the selection');
    assert.equal(b.state.bulkAnchorId, null);
});

test('pakx: e archives the whole selection — batch contract + one undo entry', async () => {
    const b = makeBulk({ emails: [email('e-1'), email('e-2'), email('e-3')] });
    b.handleNormalModeKey(key('x'));       // e-1
    b.handleNormalModeKey(key('j'));
    b.handleNormalModeKey(key('j'));
    b.handleNormalModeKey(key('x'));       // e-3
    b.handleNormalModeKey(key('e'));       // archive the selection
    assert.deepEqual(
        b.state.emails.map((e) => e.id),
        ['e-2'],
        'both selected emails must leave the list immediately',
    );
    assert.deepEqual(
        b.apiCalls.map((c) => c.path).sort(),
        ['/emails/e-1/archive', '/emails/e-3/archive'],
        'the batch must POST the existing per-email archive endpoint once per id',
    );
    assert.equal(b.state.bulkSelected.size, 0, 'the selection must clear after acting');
    assert.equal(b.state.undoStack.length, 1, 'ONE undo entry must cover the whole batch');
    assert.equal(b.state.undoStack[0].entries.length, 2);
    // The keystroke path fires bulkEmailAction without awaiting it; flush the
    // allSettled round-trip before asserting the release.
    await new Promise((resolve) => setImmediate(resolve));
    assert.equal(b.refillSuppressedIds.size, 0, 'suppression must release once the batch settles');
});

test('pakx: palette Trash routes through the same bulk path (sefy invariant)', () => {
    const b = makeBulk({ emails: [email('e-1'), email('e-2')] });
    b.toggleBulkSelect();
    b.handleNormalModeKey(key('j'));
    b.toggleBulkSelect();
    // actionSelected is what the palette's 'trash' case calls.
    b.actionSelected('trash');
    assert.deepEqual(b.state.emails, [], 'the selection must trash as a batch');
    assert.deepEqual(
        b.apiCalls.map((c) => c.path).sort(),
        ['/emails/e-1/trash', '/emails/e-2/trash'],
    );
});

test('pakx: a partial failure reverts only the failed emails and trims the undo entry', async () => {
    const b = makeBulk(
        { emails: [email('e-1'), email('e-2'), email('e-3')] },
        {
            apiImpl: (method, path) =>
                path.startsWith('/emails/e-3/')
                    ? Promise.reject(new Error('boom'))
                    : Promise.resolve({}),
        },
    );
    b.toggleBulkSelect();                  // e-1
    b.state.selectedIndex = 2;
    b.toggleBulkSelect();                  // e-3
    await b.bulkEmailAction('archive');
    assert.deepEqual(
        b.state.emails.map((e) => e.id),
        ['e-2', 'e-3'],
        'the failed email must come back at its position; the archived one stays gone',
    );
    assert.equal(b.state.undoStack.length, 1);
    assert.deepEqual(
        b.state.undoStack[0].entries.map((en) => en.emailId),
        ['e-1'],
        'the undo entry must only cover emails that actually left',
    );
    assert.equal(b.refillSuppressedIds.size, 0, 'suppression must release for settled AND reverted ids');
});

test('pakx: one undo restores the whole batch to the source mailbox', async () => {
    const work = { id: 'm-work', role: null, name: 'Work' };
    const b = makeBulk({
        mailboxes: [{ id: 'm-inbox', role: 'inbox', name: 'Inbox' }, work],
        currentMailbox: work,
        emails: [email('e-1'), email('e-2'), email('e-3')],
    });
    b.toggleBulkSelect();
    b.state.selectedIndex = 1;
    b.toggleBulkSelect();
    await b.bulkEmailAction('archive');
    assert.deepEqual(b.state.emails.map((e) => e.id), ['e-3']);
    b.apiCalls.length = 0;
    await b.performUndo();
    assert.deepEqual(
        b.state.emails.map((e) => e.id),
        ['e-1', 'e-2', 'e-3'],
        'undo must re-insert every batch member at its original position',
    );
    assert.deepEqual(b.apiCalls, [
        { method: 'POST', path: '/emails/e-1/move', body: { mailbox_id: 'm-work' } },
        { method: 'POST', path: '/emails/e-2/move', body: { mailbox_id: 'm-work' } },
    ], 'undo must move each batch member back to the mailbox it was taken from');
});

test('pakx: u toggles read state per selected email, mirroring the single u', async () => {
    const b = makeBulk({
        emails: [email('e-1', { isUnread: true }), email('e-2'), email('e-3')],
    });
    b.toggleBulkSelect();                  // e-1 (unread)
    b.state.selectedIndex = 1;
    b.toggleBulkSelect();                  // e-2 (read)
    b.handleNormalModeKey(key('u'));
    assert.equal(b.state.emails[0].isUnread, false, 'the unread one flips read');
    assert.equal(b.state.emails[1].isUnread, true, 'the read one flips unread');
    assert.deepEqual(
        b.apiCalls.map((c) => c.path).sort(),
        ['/emails/e-1/mark-read', '/emails/e-2/mark-unread'],
        'each email must POST the endpoint matching its own previous state',
    );
    assert.equal(b.state.bulkSelected.size, 2, 'a non-removing action must keep the selection');
});

test('pakx: s stars the selection through the toggle-flag contract', () => {
    const b = makeBulk({ emails: [email('e-1'), email('e-2', { isFlagged: true })] });
    b.selectAllVisible();
    b.handleNormalModeKey(key('s'));
    assert.equal(b.state.emails[0].isFlagged, true);
    assert.equal(b.state.emails[1].isFlagged, false, 'toggle semantics: each flips its own state');
    assert.deepEqual(
        b.apiCalls.map((c) => c.path).sort(),
        ['/emails/e-1/toggle-flag', '/emails/e-2/toggle-flag'],
    );
});

test('pakx: v with a selection opens the picker in bulk mode and moves the batch', () => {
    const work = { id: 'm-work', role: null, name: 'Work' };
    const b = makeBulk({
        mailboxes: [{ id: 'm-inbox', role: 'inbox', name: 'Inbox' }, work],
        emails: [email('e-1'), email('e-2'), email('e-3')],
    });
    b.toggleBulkSelect();
    b.state.selectedIndex = 1;
    b.toggleBulkSelect();
    b.handleNormalModeKey(key('v'));
    assert.equal(b.state.movePickerOpen, true);
    assert.match(b.els.moveModal.innerHTML, /2 selected/, 'the picker title must show the batch size');
    b.confirmMovePicker(work);
    assert.deepEqual(b.state.emails.map((e) => e.id), ['e-3']);
    assert.deepEqual(b.apiCalls, [
        { method: 'POST', path: '/emails/e-1/move', body: { mailbox_id: 'm-work' } },
        { method: 'POST', path: '/emails/e-2/move', body: { mailbox_id: 'm-work' } },
    ]);
    assert.equal(b.state.undoStack.length, 1, 'a bulk move must be one undo entry');
    assert.equal(b.state.undoStack[0].action, 'bulk-moved');
});

test('pakx: mailbox, split, and account switches clear the selection', () => {
    const deps = {
        updateMailboxNameDisplay: () => {}, renderMailboxes: () => {}, renderSplitTabs: () => {},
        updateActiveFilters: () => {}, loadEmails: () => {}, loadSplitCounts: () => {},
        renderSortToggle: () => {}, renderAccounts: () => {}, loadMailboxes: () => {},
        loadIdentities: () => {}, loadSplits: () => {}, authorizeAccountFromBanner: () => {},
        els: { emailList: makeEl() },
    };
    for (const [decl, run] of [
        ['function selectMailbox(', (fn, st) => fn({ id: 'm-x', role: null, name: 'X' })],
        ['function selectSplit(', (fn) => fn('all')],
        ['function selectAccount(', (fn) => fn({ id: 'a2', email: 'two@x.com' })],
    ]) {
        const state = makeState({ bulkSelected: new Set(['e-1']), bulkAnchorId: 'e-1' });
        const code = extractFunction(APP_JS, decl)
            + `\nreturn ${decl.slice('function '.length, decl.length - 1)};`;
        const names = ['state', ...Object.keys(deps)];
        // eslint-disable-next-line no-new-func
        const fn = new Function(...names, 'let lastRenderedContext = null;\n' + code)(
            state, ...Object.values(deps),
        );
        run(fn, state);
        assert.equal(state.bulkSelected.size, 0, `${decl} must clear the bulk selection`);
        assert.equal(state.bulkAnchorId, null, `${decl} must clear the range anchor`);
    }
});

test('pakx: palette offers Select All with rows, batch commands only with a selection', () => {
    const region = extractGetCommands(APP_JS);
    const load = (state, visibleRows, bulkIdsFn) =>
        // eslint-disable-next-line no-new-func
        new Function('state', 'visibleRows', 'bulkIds', region + '\nreturn getCommands;')(
            state, visibleRows, bulkIdsFn,
        )();
    const rows = [{ emailId: 'e-1' }, { emailId: 'e-2' }];
    const none = load(makeState({}), () => rows, () => []).map((c) => c.action);
    assert.ok(none.includes('select-all'), 'rows on screen → offer Select All in View');
    assert.ok(!none.some((a) => a.startsWith('bulk-')), 'no selection → no batch commands');

    const cmds = load(makeState({}), () => rows, () => ['e-1', 'e-2']);
    const actions = cmds.map((c) => c.action);
    for (const a of ['bulk-archive', 'bulk-trash', 'bulk-unread', 'bulk-flag', 'bulk-move', 'bulk-clear']) {
        assert.ok(actions.includes(a), `selection active → palette must offer ${a}`);
    }
    const archiveCmd = cmds.find((c) => c.action === 'bulk-archive');
    assert.match(archiveCmd.name, /2/, 'batch command names must show the selection count');

    const empty = load(makeState({}), () => [], () => []).map((c) => c.action);
    assert.ok(!empty.includes('select-all'), 'no rows → nothing to select');
});

test('pakx: rendered rows carry the selection marker and the bar shows the count', () => {
    const state = makeState({
        emails: [email('e-1'), email('e-2'), email('e-3')],
        bulkSelected: new Set(['e-1', 'e-3']),
    });
    const els = { emailList: makeEl(), bulkBar: makeEl() };
    const escDoc = makeEscapeDocument();
    const code = [
        'let lastRenderedContext = null;',
        extractFunction(APP_JS, 'function escapeHtml('),
        extractFunction(APP_JS, 'function escapeAttr('),
        extractFunction(APP_JS, 'function visibleRows('),
        extractFunction(APP_JS, 'function renderBulkBar('),
        extractFunction(APP_JS, 'function renderEmailList('),
        'return renderEmailList;',
    ].join('\n');
    // eslint-disable-next-line no-new-func
    const render = new Function(
        'state', 'els', 'document', 'splitCacheKey', 'renderReminderList',
        'formatDate', 'getDateGroup', 'getRecipientBadge', 'renderInviteChip',
        'scrollSelectedIntoView',
        code,
    )(
        state, els, escDoc, () => 'ctx', () => {},
        () => 'now', () => 'Today', () => null, () => '',
        () => {},
    );
    render();
    const selectedRows = els.emailList.innerHTML.match(/bulk-selected/g) || [];
    assert.equal(selectedRows.length, 2, 'exactly the selected rows must carry the marker class');
    assert.equal(els.bulkBar.classList.contains('hidden'), false, 'the bar must show with a selection');
    assert.match(els.bulkBar.textContent, /2 selected/, 'the bar must show the live count');
});

test('pakx perf: renderEmailList with 1,000 emails / 500 selected stays under 150ms', () => {
    // Budget per the kata plan's perf table (~5x local headroom for CI).
    const emails = [];
    const selected = new Set();
    for (let i = 0; i < 1000; i++) {
        emails.push(email(`e-${i}`));
        if (i % 2 === 0) selected.add(`e-${i}`);
    }
    const state = makeState({ emails, bulkSelected: selected });
    const els = { emailList: makeEl(), bulkBar: makeEl() };
    const escDoc = makeEscapeDocument();
    const code = [
        'let lastRenderedContext = null;',
        extractFunction(APP_JS, 'function escapeHtml('),
        extractFunction(APP_JS, 'function escapeAttr('),
        extractFunction(APP_JS, 'function visibleRows('),
        extractFunction(APP_JS, 'function renderBulkBar('),
        extractFunction(APP_JS, 'function renderEmailList('),
        'return renderEmailList;',
    ].join('\n');
    // eslint-disable-next-line no-new-func
    const render = new Function(
        'state', 'els', 'document', 'splitCacheKey', 'renderReminderList',
        'formatDate', 'getDateGroup', 'getRecipientBadge', 'renderInviteChip',
        'scrollSelectedIntoView',
        code,
    )(
        state, els, escDoc, () => 'ctx', () => {},
        () => 'now', () => 'Today', () => null, () => '',
        () => {},
    );
    const start = process.hrtime.bigint();
    render();
    const elapsedMs = Number(process.hrtime.bigint() - start) / 1e6;
    assert.match(els.bulkBar.textContent, /500 selected/);
    assert.ok(
        (els.emailList.innerHTML.match(/bulk-selected/g) || []).length === 500,
        'all 500 selected rows must render the marker',
    );
    assert.ok(elapsedMs < 150, `renderEmailList over 1,000/500 took ${elapsedMs.toFixed(1)}ms (budget 150ms)`);
});

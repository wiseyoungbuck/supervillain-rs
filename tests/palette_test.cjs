// Behavioral tests for the command-palette context pass (kata sefy).
//
// The repo's other JS tests (src/routes.rs #[cfg(test)]) are string-invariant:
// they pin code shapes because there was no JS harness. Node is available on
// this machine, so these tests do better — they extract the REAL getCommands
// (and its per-view builder commandsForView) from static/app.js, stand up a
// stub `state` + `visibleRows`, and assert the RUNTIME action set per view —
// not just the string shape — so a context regression can't hide behind a
// matching code form.
//
//   compose              → offers Send + Close Draft (draft-only commands)
//   detail + calendar    → offers an RSVP action (calendar-gated, like y/n/m)
//   detail, no calendar  → offers NO RSVP action (the gate mirrors keydown)
//   list, no selection   → offers NO Reply (Reply is detail-only)
//   detail               → ranks Reply above Compose (boost the view-native)
//
// Run:  node --test tests/palette_test.cjs
// Wired into cargo test via tests/palette_test.rs (mirrors email_iframe_test).

const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const APP_JS = fs.readFileSync(
    path.join(__dirname, '..', 'static', 'app.js'),
    'utf8',
);

// Extract `function commandsForView(...) { ... } function getCommands(...) { ... }`
// from app.js as one region, so the evaled getCommands can call commandsForView
// in the same scope. Mirrors the Rust `js_fn_body` helper's column-0-brace
// rule: getCommands's closing brace is the only `}` at column 0 in the region,
// so the first "\n}" after its decl is the close.
function extractGetCommands(src) {
    const cfStart = src.indexOf('function commandsForView');
    assert.notStrictEqual(cfStart, -1, 'function commandsForView must exist in app.js');
    const gcStart = src.indexOf('function getCommands', cfStart);
    assert.notStrictEqual(gcStart, -1, 'function getCommands must exist in app.js (after commandsForView)');
    const close = src.indexOf('\n}', gcStart);
    assert.notStrictEqual(close, -1, 'getCommands must close with a column-0 brace');
    // `close` is the index of '\n'; the '}' is at close+1, so slice through
    // close+2 to include getCommands's closing brace (we eval, so we need it).
    return src.slice(cfStart, close + 2);
}

// Load the real getCommands with `state` and `visibleRows` injected as
// PARAMETERS (not globalThis) so Node's own globals are untouched. The evaled
// getCommands closes over the injected `state`; commandsForView reads `state`
// (and calls `visibleRows()`) from the same closure.
function loadGetCommands(state, visibleRows) {
    const code = extractGetCommands(APP_JS);
    // eslint-disable-next-line no-new-func
    return new Function('state', 'visibleRows', code + '\nreturn getCommands;')(state, visibleRows);
}

// Extract one real column-0 function body from app.js. This follows the same
// js_fn_body convention as the Rust contract tests and extractGetCommands.
function extractFunction(src, declaration) {
    const start = src.indexOf(declaration);
    assert.notStrictEqual(start, -1, `${declaration} must exist in app.js`);
    const close = src.indexOf('\n}', start);
    assert.notStrictEqual(close, -1, `${declaration} must close with a column-0 brace`);
    return src.slice(start, close + 2);
}

// escapeHtml relies on the browser's textContent -> innerHTML serialization.
// Keep this shim exact: encode text metacharacters, but leave quotes for
// escapeAttr to encode according to the surrounding attribute context.
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

// Evaluate the real renderCommandPalette together with the real escape
// helpers. commandResults intentionally records the assigned markup before a
// browser parses it, which lets the tests prove the single-quote escapeAttr
// branch emitted &#39; as well as checking that tags are text.
function renderCommands(state, getCommands, query = '') {
    const code = [
        extractFunction(APP_JS, 'function renderCommandPalette('),
        extractFunction(APP_JS, 'function escapeHtml('),
        extractFunction(APP_JS, 'function escapeAttr('),
        'return renderCommandPalette;',
    ].join('\n');
    const commandResults = {
        innerHTML: '',
        querySelectorAll() { return []; },
    };
    const els = {
        commandInput: { value: query },
        commandResults,
    };
    // eslint-disable-next-line no-new-func
    const render = new Function('state', 'els', 'document', 'getCommands', code)(
        state,
        els,
        makeEscapeDocument(),
        getCommands,
    );
    render();
    return commandResults.innerHTML;
}

// Load the real open/close functions in one closure, just as they live in
// app.js. The two `let`s mirror their module state; all behavior under test is
// still the production function body extracted with the js_fn_body pattern.
function loadPaletteLifecycle(state, els, document, renderCommandPalette, setMode) {
    const code = [
        'let commandPalettePreviousFocus = null;',
        "let commandPalettePreviousMode = 'normal';",
        extractFunction(APP_JS, 'function openCommandPalette('),
        extractFunction(APP_JS, 'function closeCommandPalette('),
        'return { openCommandPalette, closeCommandPalette };',
    ].join('\n');
    // eslint-disable-next-line no-new-func
    return new Function(
        'state',
        'els',
        'document',
        'renderCommandPalette',
        'setMode',
        code,
    )(state, els, document, renderCommandPalette, setMode);
}

// Minimal stub state: every field commandsForView reads (view, selectedIndex,
// currentEmail, accounts, splits, mailboxes, currentMailbox, undoStack,
// selectedAccountId). visibleRows is injected separately because it is a
// module-level function in app.js, not a state field.
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
            undoStack: [],
            selectedAccountId: null,
        },
        overrides || {},
    );
}

function actionsFor(state, visibleRows) {
    const getCommands = loadGetCommands(state, visibleRows);
    return getCommands().map((c) => c.action);
}

test('sefy: compose view exposes Send and Close Draft', () => {
    const state = makeState({ view: 'compose' });
    const a = actionsFor(state, () => []);
    assert.ok(a.includes('send'), "compose view must offer Send (action 'send')");
    assert.ok(a.includes('close-draft'), "compose view must offer Close Draft (action 'close-draft')");
});

test('sefy: detail view with a calendar event exposes an RSVP action', () => {
    const state = makeState({ view: 'detail', currentEmail: { calendarEvent: { method: 'REQUEST' } } });
    const a = actionsFor(state, () => []);
    assert.ok(
        a.some((x) => x.startsWith('rsvp')),
        'detail view with a calendar event must offer an RSVP action (gated like the y/n/m keybindings)',
    );
});

test('sefy: detail view without a calendar event omits RSVP actions', () => {
    const state = makeState({ view: 'detail', currentEmail: {} });
    const a = actionsFor(state, () => []);
    assert.ok(
        !a.some((x) => x.startsWith('rsvp')),
        'detail view without a calendar event must NOT offer RSVP — the palette must mirror the keydown gate or it offers an action that no-ops',
    );
});

test('sefy: list view with no selection omits Reply', () => {
    const state = makeState({ view: 'list', selectedIndex: 0 });
    const a = actionsFor(state, () => []); // visibleRows() -> [] : no selection
    assert.ok(
        !a.includes('reply'),
        'list view with no row selected must not offer Reply — Reply is detail-only',
    );
});

test('sefy: detail view ranks Reply above Compose', () => {
    const state = makeState({ view: 'detail', currentEmail: {} });
    const a = actionsFor(state, () => []);
    const replyIdx = a.indexOf('reply');
    const composeIdx = a.indexOf('compose');
    assert.notStrictEqual(replyIdx, -1, 'detail view must include Reply');
    assert.notStrictEqual(composeIdx, -1, 'detail view must include Compose (global tail)');
    assert.ok(replyIdx < composeIdx, 'detail view must rank Reply above Compose (boost the view-native command)');
});

test('sefy: settings view is a low-action surface (Add Account / Remove Account / Help)', () => {
    const state = makeState({
        view: 'settings',
        accounts: [{ id: 'acc-1', email: 'a@example.com' }],
    });
    const a = actionsFor(state, () => []);
    assert.ok(a.includes('add-account'), 'settings must offer Add Account');
    assert.ok(a.includes('remove-account:acc-1'), 'settings must offer Remove Account per account');
    assert.ok(a.includes('help'), 'settings must offer Help');
    // Settings is a low-action surface — no mail actions leak in.
    assert.ok(!a.includes('reply'), 'settings must not offer Reply');
    assert.ok(!a.includes('archive'), 'settings must not offer Archive');
});

test('sefy: list view exposes a Delete Split command per split (roborev 375)', () => {
    // Splits manifest as tabs on the list view, so deleting one is a
    // list-screen action. The context pass dropped the per-split Delete
    // Split commands entirely, leaving deleteSplit unreachable — this pins
    // that they're emitted in the list branch (roborev 375, High).
    const state = makeState({
        view: 'list',
        selectedIndex: 0,
        splits: [{ id: 'work', name: 'Work' }, { id: 'news', name: 'News' }],
    });
    const a = actionsFor(state, () => []);
    assert.ok(
        a.includes('delete-split:work'),
        "list view must offer a Delete Split command per split (action 'delete-split:<id>') (roborev 375)",
    );
    assert.ok(
        a.includes('delete-split:news'),
        'list view must offer a Delete Split command for every split, not just the first (roborev 375)',
    );
});

test('sefy: list view with no splits offers no Delete Split command', () => {
    const state = makeState({ view: 'list', selectedIndex: 0, splits: [] });
    const a = actionsFor(state, () => []);
    assert.ok(
        !a.some((x) => x.startsWith('delete-split:')),
        'list view with no splits must not offer any Delete Split command',
    );
});

test('sefy: Remove Account is settings-only (not offered on list/detail) (roborev 375)', () => {
    // The context pass scoped Remove Account to settings (previously it sat in
    // the flat global list, available on every screen). That scoping is
    // intentional — account teardown is a settings action — so pin that list
    // and detail do NOT offer it even when accounts exist (roborev 375).
    const acct = { id: 'acc-1', email: 'a@example.com' };
    for (const view of ['list', 'detail']) {
        const state = makeState({ view, selectedIndex: 0, currentEmail: {}, accounts: [acct] });
        const a = actionsFor(state, () => []);
        assert.ok(
            !a.some((x) => x.startsWith('remove-account:')),
            `${view} view must not offer Remove Account — account teardown is settings-only (roborev 375)`,
        );
    }
});

test('sefy: unknown view falls back to the defensive global set (roborev 378 #6)', () => {
    // commandsForView's default branch is the defensive fallback for an
    // unrecognized view — the global set with no view-native commands. Pin
    // that it returns the global actions and omits reply/rsvp/archive so a
    // future view can't accidentally inherit a named branch's commands via
    // the default (roborev 378 #6).
    const state = makeState({ view: 'bogus', selectedIndex: 0, currentEmail: {} });
    const a = actionsFor(state, () => []);
    assert.ok(a.includes('compose'), 'default branch must offer the global Compose command');
    assert.ok(a.includes('help'), 'default branch must offer the global Help command');
    assert.ok(!a.includes('reply'), 'default branch must not offer Reply — that is detail-only');
    assert.ok(
        !a.some((x) => x.startsWith('rsvp')),
        'default branch must not offer RSVP — that is detail + calendar-gated',
    );
    assert.ok(!a.includes('archive'), 'default branch must not offer Archive — that is view-native');
});

test('fhtz: split command names render markup as text and escape quote-bearing ids', () => {
    const state = makeState({
        view: 'list',
        commandPaletteIndex: 0,
        splits: [{
            id: `split\"double'single`,
            name: '<img src=x onerror="window.__xss_palette=1">',
        }],
    });
    const getCommands = loadGetCommands(state, () => []);
    const markup = renderCommands(state, getCommands);

    assert.ok(markup.includes('Delete Split: &lt;img src=x onerror="window.__xss_palette=1"&gt;'));
    assert.ok(!markup.includes('<img'), 'the split-name payload must not survive as a live tag');
    assert.ok(markup.includes('&quot;double'), 'escapeAttr must encode the double-quote branch');
    assert.ok(markup.includes('&#39;single'), 'escapeAttr must encode the single-quote branch');
});

test('fhtz: account labels containing markup render as literal command text', () => {
    const state = makeState({
        view: 'settings',
        commandPaletteIndex: 0,
        accounts: [{
            id: 'acct-1',
            email: '<svg onload="window.__xss_account=1">',
        }],
    });
    const getCommands = loadGetCommands(state, () => []);
    const markup = renderCommands(state, getCommands);

    assert.ok(markup.includes('Remove Account: &lt;svg onload="window.__xss_account=1"&gt;'));
    assert.ok(!markup.includes('<svg'), 'the account-label payload must not survive as a live tag');
});

test('sqke: cancelling the palette restores the insert-mode field and focus', () => {
    const state = { mode: 'insert', commandPaletteIndex: 0 };
    let restored = 0;
    const previousField = {
        isConnected: true,
        focus() {
            restored++;
            document.activeElement = previousField;
            // Dense settings inputs do not have a focus listener. The captured
            // mode must therefore provide the insert-mode fallback.
        },
    };
    const document = { activeElement: previousField };
    const classes = new Set(['hidden']);
    const els = {
        commandPalette: {
            classList: {
                add(value) { classes.add(value); },
                remove(value) { classes.delete(value); },
            },
        },
        commandInput: {
            value: 'stale',
            focus() {
                document.activeElement = this;
                state.mode = 'normal'; // model the previous field's blur
            },
        },
    };
    const setMode = (mode) => { state.mode = mode; };
    const lifecycle = loadPaletteLifecycle(state, els, document, () => {}, setMode);

    lifecycle.openCommandPalette();
    assert.equal(state.mode, 'command');
    assert.equal(document.activeElement, els.commandInput);
    lifecycle.closeCommandPalette({ cancelled: true });
    assert.equal(restored, 1, 'Escape/cancel must refocus the previous field');
    assert.equal(document.activeElement, previousField);
    assert.equal(state.mode, 'insert', 'cancel must restore the captured insert mode');

    lifecycle.openCommandPalette();
    lifecycle.closeCommandPalette();
    assert.equal(restored, 1, 'an executed action close must not restore stale focus');
    assert.equal(state.mode, 'normal', 'action close retains the normal fallback');
});

test('sqke: cancelling after the previous field disconnects falls back to normal mode', () => {
    const state = { mode: 'insert', commandPaletteIndex: 0 };
    let focusCalls = 0;
    const previousField = {
        isConnected: true,
        focus() { focusCalls++; },
    };
    const document = { activeElement: previousField };
    const els = {
        commandPalette: {
            classList: {
                add() {},
                remove() {},
            },
        },
        commandInput: {
            value: '',
            focus() {
                document.activeElement = this;
            },
        },
    };
    const setMode = (mode) => { state.mode = mode; };
    const lifecycle = loadPaletteLifecycle(state, els, document, () => {}, setMode);

    lifecycle.openCommandPalette();
    previousField.isConnected = false;
    lifecycle.closeCommandPalette({ cancelled: true });

    assert.equal(focusCalls, 0, 'a disconnected field cannot be refocused');
    assert.equal(state.mode, 'normal', 'body focus must not remain in insert mode');
});

test('sqke: Enter with no selected command closes as a cancellation', () => {
    const state = { commandPaletteIndex: 0 };
    const els = {
        commandResults: {
            querySelector() { return null; },
        },
    };
    let closeOptions;
    let executeCalls = 0;
    let prevented = false;
    const handlerCode = extractFunction(APP_JS, 'function handleCommandPaletteKey(')
        + '\nreturn handleCommandPaletteKey;';
    // eslint-disable-next-line no-new-func
    const handleCommandPaletteKey = new Function(
        'state',
        'els',
        'closeCommandPalette',
        'executeCommand',
        'renderCommandPalette',
        handlerCode,
    )(
        state,
        els,
        (options) => { closeOptions = options; },
        () => { executeCalls++; },
        () => {},
    );

    handleCommandPaletteKey({
        key: 'Enter',
        preventDefault() { prevented = true; },
    });

    assert.deepEqual(closeOptions, { cancelled: true });
    assert.equal(executeCalls, 0, 'no command may execute without a selection');
    assert.equal(prevented, true);
});

test('sqke: Ctrl+Enter in settings normal mode does not enter edit mode', () => {
    const state = { selectedAccountId: 'acct-1', settingsMode: 'view' };
    const els = {
        acctConfirmDelete: {
            classList: { contains(value) { return value === 'hidden'; } },
        },
    };
    const code = extractFunction(APP_JS, 'function handleSettingsNormalKey(')
        + '\nreturn handleSettingsNormalKey;';
    // eslint-disable-next-line no-new-func
    const handleSettingsNormalKey = new Function('state', 'els', code)(state, els);
    let prevented = false;

    handleSettingsNormalKey({
        key: 'Enter',
        ctrlKey: true,
        metaKey: false,
        altKey: false,
        preventDefault() { prevented = true; },
    });

    assert.equal(state.settingsMode, 'view');
    assert.equal(prevented, false, 'Ctrl+Enter must fall through in settings normal mode');
});

test('sqke: repeated ArrowDown stays on the final filtered command', () => {
    const state = makeState({ view: 'compose', commandPaletteIndex: 0 });
    const getCommands = loadGetCommands(state, () => []);
    const commands = getCommands();
    state.commandPaletteIndex = commands.length - 1;

    const handlerCode = extractFunction(APP_JS, 'function handleCommandPaletteKey(')
        + '\nreturn handleCommandPaletteKey;';
    const rerender = () => { renderCommands(state, getCommands); };
    // eslint-disable-next-line no-new-func
    const handleCommandPaletteKey = new Function(
        'state',
        'renderCommandPalette',
        handlerCode,
    )(state, rerender);

    let prevented = false;
    handleCommandPaletteKey({
        key: 'ArrowDown',
        preventDefault() { prevented = true; },
    });
    assert.equal(state.commandPaletteIndex, commands.length - 1);
    assert.equal(prevented, true);
});

// ---------------------------------------------------------------------------
// map4: palette completeness — every audited app action is reachable from the
// palette. The offering tests assert commandsForView emits each command in the
// right view under the right state gate (the gate mirrors the key/click path's
// own no-op condition). The exec tests drive the REAL executeCommand together
// with the REAL target function — only leaf dependencies (api, DOM renderers,
// data loaders) are recording stubs — and assert the observable state change
// or request contract, not "the right function was called".

// Assemble the real executeCommand plus the named real functions extracted
// from app.js, with everything else injected as parameters. Identifiers in
// branches a test never executes are simply never resolved.
function loadExecutor(declarations, deps, preamble = '') {
    const code = [
        preamble,
        ...declarations.map((d) => extractFunction(APP_JS, d)),
        extractFunction(APP_JS, 'function executeCommand('),
        'return executeCommand;',
    ].join('\n');
    const names = Object.keys(deps);
    // eslint-disable-next-line no-new-func
    return new Function(...names, code)(...names.map((n) => deps[n]));
}

test('map4: list and detail offer Undo only when the undo stack is non-empty', () => {
    for (const view of ['list', 'detail']) {
        const base = { view, currentEmail: view === 'detail' ? {} : null };
        assert.ok(
            !actionsFor(makeState(base), () => []).includes('undo'),
            `${view} view must not offer Undo with an empty undo stack (same gate as the undo button)`,
        );
        assert.ok(
            actionsFor(makeState({ ...base, undoStack: [{ action: 'archived' }] }), () => []).includes('undo'),
            `${view} view must offer Undo once something is undoable`,
        );
    }
});

test('map4: Unsubscribe & Archive All needs a selection in list, is always on in detail', () => {
    assert.ok(
        !actionsFor(makeState({ view: 'list' }), () => []).includes('unsubscribe'),
        'list with no selected row must not offer Unsubscribe (unsubscribeAndArchiveAll no-ops without a selection)',
    );
    assert.ok(
        actionsFor(makeState({ view: 'list' }), () => [{ emailId: 'e-1' }]).includes('unsubscribe'),
        'list with a selected row must offer Unsubscribe & Archive All',
    );
    assert.ok(
        actionsFor(makeState({ view: 'detail', currentEmail: { id: 'e-1' } }), () => []).includes('unsubscribe'),
        'detail must offer Unsubscribe & Archive All (the open email is the selection)',
    );
});

test('map4: Open Settings is offered from list and detail (the g-s chord surface)', () => {
    for (const view of ['list', 'detail']) {
        const a = actionsFor(makeState({ view, currentEmail: {} }), () => []);
        assert.ok(a.includes('open-settings'), `${view} view must offer Open Settings`);
    }
});

test('map4: Switch Account is emitted per account on list and detail, never settings', () => {
    const accounts = [{ id: 'a1', email: 'one@x.com' }, { id: 'a2', email: 'two@x.com' }];
    for (const view of ['list', 'detail']) {
        const a = actionsFor(makeState({ view, currentEmail: {}, accounts }), () => []);
        assert.ok(
            a.includes('switch-account:a1') && a.includes('switch-account:a2'),
            `${view} view must emit one Switch Account command per account`,
        );
    }
    const s = actionsFor(makeState({ view: 'settings', accounts }), () => []);
    assert.ok(
        !s.some((x) => x.startsWith('switch-account:')),
        'settings disables 1-9 account switching; the palette must mirror that gate',
    );
});

test('map4: split navigation commands are offered only on the inbox with splits', () => {
    const splits = [{ id: 'work', name: 'Work' }, { id: 'news', name: 'News' }];
    const a = actionsFor(
        makeState({ view: 'list', currentMailbox: { role: 'inbox' }, splits }),
        () => [],
    );
    assert.ok(a.includes('go-split:work') && a.includes('go-split:news'), 'one Go to Split per split');
    assert.ok(a.includes('next-split') && a.includes('prev-split'), 'Tab-cycling must be palette-reachable');
    const archive = actionsFor(
        makeState({ view: 'list', currentMailbox: { role: 'archive' }, splits }),
        () => [],
    );
    assert.ok(
        !archive.some((x) => x.startsWith('go-split:')) && !archive.includes('next-split'),
        'cycleSplit/selectSplitByIndex no-op outside the inbox; the palette must not offer them there',
    );
    const noSplits = actionsFor(
        makeState({ view: 'list', currentMailbox: { role: 'inbox' } }),
        () => [],
    );
    assert.ok(!noSplits.includes('next-split'), 'no splits → nothing to cycle');
});

test('map4: Back to List is detail-only', () => {
    assert.ok(
        actionsFor(makeState({ view: 'detail', currentEmail: {} }), () => []).includes('back-to-list'),
        'detail must offer Back to List (the q/Esc path)',
    );
    assert.ok(
        !actionsFor(makeState({ view: 'list' }), () => []).includes('back-to-list'),
        'list must not offer Back to List — it is already the list',
    );
});

test('map4: Set Default Account is settings-only, gated on a selected account', () => {
    assert.ok(
        actionsFor(makeState({ view: 'settings', selectedAccountId: 'a1' }), () => []).includes('set-default-account'),
        'settings with a selected account must offer Set Default Account (the Shift+D path)',
    );
    assert.ok(
        !actionsFor(makeState({ view: 'settings' }), () => []).includes('set-default-account'),
        "Shift+D no-ops without a selected account; the palette must mirror that gate",
    );
    assert.ok(
        !actionsFor(makeState({ view: 'list', selectedAccountId: 'a1' }), () => []).includes('set-default-account'),
        'Set Default Account is a settings action, not a list action',
    );
});

test('map4: starred filter and sort toggles are list commands gated on a mailbox', () => {
    const a = actionsFor(makeState({ view: 'list', currentMailbox: { role: 'inbox' } }), () => []);
    assert.ok(a.includes('toggle-starred'), 'list must offer Toggle Starred Only (click-only until now)');
    assert.ok(a.includes('toggle-sort'), 'list must offer Toggle Sort Order (click-only until now)');
    const none = actionsFor(makeState({ view: 'list' }), () => []);
    assert.ok(
        !none.includes('toggle-starred') && !none.includes('toggle-sort'),
        'both toggles no-op without a current mailbox; the palette must mirror that gate',
    );
});

test('map4: Toggle Sort Order keeps the Gmail per-page caveat in its description', () => {
    const state = makeState({ view: 'list', currentMailbox: { role: 'inbox' } });
    const cmd = loadGetCommands(state, () => [])().find((c) => c.action === 'toggle-sort');
    assert.ok(cmd, 'list must offer toggle-sort');
    assert.match(
        cmd.desc,
        /per (fetched )?page/i,
        'the Gmail oldest-first-per-page caveat (roborev 291) must survive into the command description',
    );
});

test('map4: Go to Drafts/Sent/Spam are offered only where the role exists', () => {
    const mailboxes = [
        { id: 'm1', role: 'inbox' },
        { id: 'm2', role: 'drafts' },
        { id: 'm3', role: 'sent' },
    ];
    for (const view of ['list', 'detail']) {
        const a = actionsFor(makeState({ view, currentEmail: {}, mailboxes }), () => []);
        assert.ok(a.includes('go-drafts'), `${view} must offer Go to Drafts when a drafts mailbox exists`);
        assert.ok(a.includes('go-sent'), `${view} must offer Go to Sent when a sent mailbox exists`);
        assert.ok(
            !a.includes('go-spam'),
            `${view} must not offer Go to Spam when no spam mailbox exists (the palette must not offer a no-op)`,
        );
    }
});

test('map4: settings offers Timezone Settings alongside Reminder Settings', () => {
    const a = actionsFor(makeState({ view: 'settings' }), () => []);
    assert.ok(a.includes('timezone-settings'), 'settings must offer Timezone Settings');
    assert.ok(a.includes('reminder-settings'), 'Reminder Settings (the sibling) must survive');
});

test('map4 exec: Undo re-inserts the archived email and moves it back on the server', () => {
    const state = makeState({
        view: 'list',
        emails: [],
        mailboxes: [{ id: 'm-inbox', role: 'inbox' }],
        undoStack: [{ action: 'archived', emailId: 'e-9', emailData: { id: 'e-9' }, insertIndex: 0 }],
    });
    const apiCalls = [];
    const executeCommand = loadExecutor(['async function performUndo('], {
        state,
        els: { undoToast: { classList: { add() {}, remove() {} } } },
        api: (method, path, body) => { apiCalls.push({ method, path, body }); return Promise.resolve({}); },
        showStatus() {},
        refillSuppressedIds: new Set(),
        extendThreadGroups() {},
        visibleRowIndexForEmailId: () => 0,
        invalidateSplitListCache() {},
        renderEmailList() {},
        adjustSplitCounts() {},
        loadSplitCounts() {},
        loadReminders() {},
    });
    executeCommand('undo');
    assert.equal(state.undoStack.length, 0, 'Undo must consume the stack entry');
    assert.deepEqual(
        state.emails.map((e) => e.id),
        ['e-9'],
        'the archived email must be re-inserted into the list',
    );
    assert.deepEqual(
        apiCalls,
        [{ method: 'POST', path: '/emails/e-9/move', body: { mailbox_id: 'm-inbox' } }],
        'Undo must move the email back to the inbox on the server',
    );
});

test('map4 exec: Unsubscribe & Archive All removes the sender and posts the bulk archive', () => {
    const state = makeState({
        view: 'list',
        selectedIndex: 0,
        emails: [
            { id: 'e-1', from: [{ email: 'spam@x.com' }] },
            { id: 'e-2', from: [{ email: 'keep@x.com' }] },
            { id: 'e-3', from: [{ email: 'SPAM@x.com' }] },
        ],
    });
    const apiCalls = [];
    const executeCommand = loadExecutor(
        [
            'async function unsubscribeAndArchiveAll(',
            'function getSelectedEmailId(',
            'function removeEmailsFromList(',
        ],
        {
            state,
            visibleRows: () => state.emails.map((e) => ({ emailId: e.id })),
            refillSuppressedIds: new Set(),
            splitListCache: {},
            adjustSplitCounts() {},
            invalidateSplitListCache() {},
            renderEmailList() {},
            maybeRefillEmails() {},
            showStatus() {},
            goToNextEmail() {},
            api: (method, path) => {
                apiCalls.push({ method, path });
                return Promise.resolve({ archived: 2, sender: 'spam@x.com' });
            },
            loadSplitCounts() {},
        },
    );
    executeCommand('unsubscribe');
    assert.deepEqual(
        state.emails.map((e) => e.id),
        ['e-2'],
        'every email from the selected sender (case-insensitive) must leave the list immediately',
    );
    assert.deepEqual(apiCalls, [{ method: 'POST', path: '/emails/e-1/unsubscribe-and-archive-all' }]);
});

test('map4 exec: Open Settings and Back to List drive the real showView', () => {
    const classes = {};
    const mkView = (name) => ({ classList: { toggle(_cls, on) { classes[name] = on; } } });
    const state = makeState({ view: 'list' });
    const executeCommand = loadExecutor(['function openSettings(', 'function showView('], {
        state,
        els: {
            emailListView: mkView('list'),
            emailDetailView: mkView('detail'),
            composeView: mkView('compose'),
            settingsView: mkView('settings'),
        },
        renderSettings() {},
        openWizard() {},
        saveScrollPosition() {},
    });
    executeCommand('open-settings');
    assert.equal(state.view, 'settings', 'Open Settings must land on the settings screen');
    assert.equal(classes.settings, true);
    assert.equal(classes.list, false);

    state.currentEmail = { id: 'e-1' };
    state.view = 'detail';
    executeCommand('back-to-list');
    assert.equal(state.view, 'list', 'Back to List must return to the list view');
    assert.equal(classes.list, true);
});

test('map4 exec: Switch Account changes the live account through the real selectAccount', () => {
    const accounts = [{ id: 'a1', email: 'one@x.com' }, { id: 'a2', email: 'two@x.com' }];
    const state = makeState({
        view: 'list',
        accounts,
        currentAccount: accounts[0],
        emails: [{ id: 'stale' }],
        currentMailbox: { role: 'inbox' },
    });
    const executeCommand = loadExecutor(['function selectAccount('], {
        state,
        els: { emailList: { innerHTML: 'stale' } },
        authorizeAccountFromBanner() {},
        renderSortToggle() {},
        renderAccounts() {},
        loadMailboxes() {},
        loadIdentities() {},
        loadSplits() {},
        showStatus() {},
    }, 'let lastRenderedContext = null;');
    executeCommand('switch-account:a2');
    assert.equal(state.currentAccount, accounts[1], 'the live account must switch');
    assert.deepEqual(state.emails, [], "the previous account's emails must clear immediately");
    assert.equal(state.currentMailbox, null, 'mailbox context resets for the new account');
});

test('map4 exec: split commands drive the real selectSplit state change', () => {
    const state = makeState({
        view: 'list',
        currentMailbox: { role: 'inbox' },
        splits: [{ id: 'work', name: 'Work' }, { id: 'news', name: 'News' }],
        currentSplit: 'all',
    });
    const executeCommand = loadExecutor(
        ['function selectSplit(', 'function cycleSplit(', 'function selectSplitByIndex('],
        { state, renderSplitTabs() {}, loadEmails() {} },
    );
    executeCommand('go-split:news');
    assert.equal(state.currentSplit, 'news', 'Go to Split must land on the named split');
    executeCommand('next-split');
    assert.equal(state.currentSplit, 'all', 'news is the last tab; Next Split wraps to All');
    executeCommand('prev-split');
    assert.equal(state.currentSplit, 'news', 'Previous Split cycles back');
});

test('map4 exec: starred and sort toggles flip the real state', () => {
    const state = makeState({
        view: 'list',
        currentMailbox: { role: 'inbox' },
        starredOnly: false,
        sortOrder: 'date_desc',
    });
    const executeCommand = loadExecutor(
        ['function toggleStarredOnly(', 'function toggleSortOrder('],
        {
            state,
            renderStarredItem() {},
            updateMailboxNameDisplay() {},
            loadEmails() {},
            loadSplitCounts() {},
            renderSortToggle() {},
        },
    );
    executeCommand('toggle-starred');
    assert.equal(state.starredOnly, true, 'Toggle Starred Only must enable the filter');
    executeCommand('toggle-sort');
    assert.equal(state.sortOrder, 'date_asc', 'Toggle Sort Order must flip to oldest-first');
    executeCommand('toggle-sort');
    assert.equal(state.sortOrder, 'date_desc', 'toggling again must flip back');
});

test('map4 exec: Go to Drafts selects the drafts mailbox through the real selectMailbox', () => {
    const drafts = { id: 'm-drafts', role: 'drafts', name: 'Drafts' };
    const state = makeState({
        view: 'list',
        mailboxes: [{ id: 'm-inbox', role: 'inbox' }, drafts],
        searchTokens: [{ type: 'text', value: 'q' }],
    });
    const executeCommand = loadExecutor(['function selectMailbox('], {
        state,
        updateMailboxNameDisplay() {},
        renderMailboxes() {},
        renderSplitTabs() {},
        updateActiveFilters() {},
        loadEmails() {},
        loadSplitCounts() {},
    });
    executeCommand('go-drafts');
    assert.equal(state.currentMailbox, drafts, 'Go to Drafts must select the drafts mailbox');
    assert.deepEqual(state.searchTokens, [], 'mailbox switch clears search tokens (same as clicking the mailbox)');
});

test('map4 exec: Set Default Account PUTs the default-account contract', () => {
    const state = makeState({ view: 'settings', selectedAccountId: 'a2' });
    const apiCalls = [];
    const executeCommand = loadExecutor(['async function setDefaultAccount('], {
        state,
        api: (method, path) => { apiCalls.push({ method, path }); return Promise.resolve({}); },
        showStatus() {},
        showFormError() {},
        loadAccounts() {},
    });
    executeCommand('set-default-account');
    assert.deepEqual(apiCalls, [{ method: 'PUT', path: '/accounts/a2/default' }]);
});

test('map4 exec: Timezone Settings lands on the settings screen at the timezone section', () => {
    const state = makeState({ view: 'settings' });
    let scrolledTo = null;
    const mkView = () => ({ classList: { toggle() {} } });
    const executeCommand = loadExecutor(['function openSettings(', 'function showView('], {
        state,
        els: {
            emailListView: mkView(),
            emailDetailView: mkView(),
            composeView: mkView(),
            settingsView: mkView(),
        },
        renderSettings() {},
        openWizard() {},
        saveScrollPosition() {},
        document: { getElementById(id) { return { scrollIntoView() { scrolledTo = id; } }; } },
    });
    executeCommand('timezone-settings');
    assert.equal(state.view, 'settings');
    assert.equal(scrolledTo, 'timezone-settings', 'the timezone section must be brought into view');
});

test('map4 perf: filtering 1,000 synthetic commands stays under the 100ms budget', () => {
    // Budget per the kata plan's perf table: 100ms with ~5x headroom over
    // local (filter+markup of 1,000 commands runs in a few ms here) so CI
    // jitter can't flake it. Synthetic commands, no network, no DOM parse —
    // renderCommandPalette's filter + innerHTML string build is the cost.
    const commands = [];
    for (let i = 0; i < 1000; i++) {
        commands.push({
            name: `Synthetic Command ${i}`,
            desc: `does synthetic thing number ${i}`,
            shortcut: '',
            action: `syn-${i}`,
        });
    }
    const state = makeState({ commandPaletteIndex: 0 });
    const code = [
        extractFunction(APP_JS, 'function renderCommandPalette('),
        extractFunction(APP_JS, 'function escapeHtml('),
        extractFunction(APP_JS, 'function escapeAttr('),
        'return renderCommandPalette;',
    ].join('\n');
    const commandResults = { innerHTML: '', querySelectorAll() { return []; } };
    const els = { commandInput: { value: '99' }, commandResults };
    // eslint-disable-next-line no-new-func
    const render = new Function('state', 'els', 'document', 'getCommands', code)(
        state,
        els,
        makeEscapeDocument(),
        () => commands,
    );
    const start = process.hrtime.bigint();
    render();
    const elapsedMs = Number(process.hrtime.bigint() - start) / 1e6;
    assert.ok(commandResults.innerHTML.includes('Synthetic Command 99'), 'the filter must match the query');
    assert.ok(
        elapsedMs < 100,
        `filter+render of 1,000 commands took ${elapsedMs.toFixed(1)}ms (budget 100ms)`,
    );
});

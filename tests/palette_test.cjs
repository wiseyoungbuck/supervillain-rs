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
// currentEmail, accounts). visibleRows is injected separately because it is a
// module-level function in app.js, not a state field.
function makeState(overrides) {
    return Object.assign(
        { view: 'list', selectedIndex: 0, currentEmail: null, accounts: [], splits: [] },
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

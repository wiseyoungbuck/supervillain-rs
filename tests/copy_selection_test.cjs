// Behavioral tests for Ctrl/Cmd+C copy out of the sandboxed email-body
// iframe (kata 26gb).
//
// The window-blur focus bounce keeps keyboard focus in the PARENT document,
// so native copy sees the parent's empty selection and silently copies
// nothing. copyEmailIframeSelection is the fix: it reads the iframe's
// selection through allow-same-origin and writes the clipboard itself.
// These tests extract the REAL functions from static/app.js, inject mock
// window/document/navigator, and assert the runtime behavior. They also pin
// the two wiring shapes the fix depends on (the handleKeyDown Ctrl+C branch
// and the handleNormalModeKey chord guard) so a refactor can't silently
// bring back "Ctrl+C opens compose".
//
// Run:  node --test tests/copy_selection_test.cjs
// Wired into cargo test via tests/copy_selection_test.rs (mirrors
// email_iframe_test).

const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const APP_JS = fs.readFileSync(
    path.join(__dirname, '..', 'static', 'app.js'),
    'utf8',
);

// Extract `function copyEmailIframeSelection ... function
// execCommandCopyFallback { ... }` from app.js as one contiguous slice (the
// fallback is defined immediately after the copier). Mirrors the column-0
// brace rule used by email_iframe_test.cjs: each function's closing brace is
// the only `}` at column 0 in its region.
function extractCopyFns(src) {
    const start = src.indexOf('function copyEmailIframeSelection');
    assert.notStrictEqual(start, -1, 'copyEmailIframeSelection must exist in app.js');
    const fbStart = src.indexOf('function execCommandCopyFallback', start);
    assert.notStrictEqual(fbStart, -1, 'execCommandCopyFallback must follow copyEmailIframeSelection');
    const close = src.indexOf('\n}', fbStart);
    assert.notStrictEqual(close, -1, 'execCommandCopyFallback must close with a column-0 brace');
    return src.slice(start, close + 2);
}

// Load the real functions with faked browser globals injected as PARAMETERS
// (not globalThis) so Node's own environment is untouched.
function loadCopyFns({ window, document, navigator }) {
    const code = extractCopyFns(APP_JS);
    // eslint-disable-next-line no-new-func
    return new Function(
        'window',
        'document',
        'navigator',
        code + '\nreturn { copyEmailIframeSelection, execCommandCopyFallback };',
    )(window, document, navigator);
}

// --- mocks -----------------------------------------------------------------

function mockSelection(text, collapsed) {
    return { isCollapsed: collapsed, toString: () => text };
}

function mockIframe(sel, { hidden = false } = {}) {
    // offsetParent is null for elements inside a display:none subtree —
    // the copier uses it to skip iframes whose views are not on screen.
    return {
        offsetParent: hidden ? null : {},
        contentWindow: { getSelection: () => sel },
    };
}

function mockEnv({ parentSel = null, iframes = [], clipboard } = {}) {
    const writes = [];
    const execCalls = [];
    const created = [];
    const focusLog = [];
    const body = {
        children: [],
        appendChild(el) { this.children.push(el); },
    };
    const document = {
        querySelectorAll: (selector) => {
            assert.strictEqual(selector, 'iframe.email-iframe');
            return iframes;
        },
        createElement: (tag) => {
            const el = {
                tag,
                value: '',
                attrs: {},
                style: {},
                setAttribute(k, v) { this.attrs[k] = v; },
                select() { el.selected = true; },
                remove() { body.children = body.children.filter(c => c !== el); },
            };
            created.push(el);
            return el;
        },
        body,
        activeElement: { focus: () => focusLog.push('restored') },
        execCommand: (cmd) => { execCalls.push(cmd); return true; },
    };
    const window = { getSelection: () => parentSel };
    const navigator = clipboard === undefined
        ? { clipboard: { writeText: (t) => { writes.push(t); return Promise.resolve(); } } }
        : clipboard;
    return { window, document, navigator, writes, execCalls, created, focusLog };
}

const flushMicrotasks = () => new Promise((r) => setTimeout(r, 0));

// --- behavior --------------------------------------------------------------

test('iframe selection is copied as plain text and the call reports true', () => {
    const env = mockEnv({
        parentSel: mockSelection('', true),
        iframes: [mockIframe(mockSelection('meeting is at 3pm', false))],
    });
    const { copyEmailIframeSelection } = loadCopyFns(env);
    assert.strictEqual(copyEmailIframeSelection(), true);
    assert.deepStrictEqual(env.writes, ['meeting is at 3pm']);
});

test('a real parent-document selection wins: no hijack, native copy owns it', () => {
    const env = mockEnv({
        parentSel: mockSelection('subject line text', false),
        iframes: [mockIframe(mockSelection('iframe text', false))],
    });
    const { copyEmailIframeSelection } = loadCopyFns(env);
    assert.strictEqual(copyEmailIframeSelection(), false);
    assert.deepStrictEqual(env.writes, []);
});

test('collapsed iframe selection means nothing to copy', () => {
    const env = mockEnv({
        parentSel: null,
        iframes: [mockIframe(mockSelection('', true))],
    });
    const { copyEmailIframeSelection } = loadCopyFns(env);
    assert.strictEqual(copyEmailIframeSelection(), false);
    assert.deepStrictEqual(env.writes, []);
});

test('no iframes at all means nothing to copy', () => {
    const env = mockEnv({ parentSel: null, iframes: [] });
    const { copyEmailIframeSelection } = loadCopyFns(env);
    assert.strictEqual(copyEmailIframeSelection(), false);
});

test('a hidden iframe\'s stale selection is never copied (q-back-to-list case)', () => {
    // Selections survive display:none — leaving detail view hides the iframe
    // but keeps its document and selection alive. Ctrl+C in the list with
    // nothing visibly selected must stay a native no-op, not clobber the
    // clipboard with stale invisible text (roborev 478).
    const env = mockEnv({
        parentSel: null,
        iframes: [mockIframe(mockSelection('stale detail text', false), { hidden: true })],
    });
    const { copyEmailIframeSelection } = loadCopyFns(env);
    assert.strictEqual(copyEmailIframeSelection(), false);
    assert.deepStrictEqual(env.writes, []);
});

test('a hidden stale iframe does not shadow a visible selection later in DOM order', () => {
    // Reply flow: the hidden detail-body iframe comes first in document
    // order and still holds its old selection; the visible compose-quote
    // iframe's selection must win (roborev 478).
    const env = mockEnv({
        parentSel: null,
        iframes: [
            mockIframe(mockSelection('stale detail text', false), { hidden: true }),
            mockIframe(mockSelection('quote text the user selected', false)),
        ],
    });
    const { copyEmailIframeSelection } = loadCopyFns(env);
    assert.strictEqual(copyEmailIframeSelection(), true);
    assert.deepStrictEqual(env.writes, ['quote text the user selected']);
});

test('a throwing iframe (detached mid-teardown) is skipped, later ones still copy', () => {
    const hostile = { offsetParent: {}, get contentWindow() { throw new Error('detached'); } };
    const env = mockEnv({
        parentSel: null,
        iframes: [hostile, mockIframe(mockSelection('still here', false))],
    });
    const { copyEmailIframeSelection } = loadCopyFns(env);
    assert.strictEqual(copyEmailIframeSelection(), true);
    assert.deepStrictEqual(env.writes, ['still here']);
});

test('writeText rejection falls back to the textarea/execCommand path', async () => {
    const env = mockEnv({
        parentSel: null,
        iframes: [mockIframe(mockSelection('fallback me', false))],
    });
    env.navigator = { clipboard: { writeText: () => Promise.reject(new Error('denied')) } };
    const { copyEmailIframeSelection } = loadCopyFns(env);
    assert.strictEqual(copyEmailIframeSelection(), true);
    await flushMicrotasks();
    assert.deepStrictEqual(env.execCalls, ['copy']);
    const ta = env.created.find(el => el.tag === 'textarea');
    assert.ok(ta, 'fallback must create a textarea');
    assert.strictEqual(ta.value, 'fallback me');
    assert.strictEqual(ta.selected, true);
    assert.deepStrictEqual(env.document.body.children, [], 'textarea must be removed after the copy');
    assert.deepStrictEqual(env.focusLog, ['restored'], 'focus must be handed back');
});

test('missing navigator.clipboard goes straight to the fallback, synchronously', () => {
    const env = mockEnv({
        parentSel: null,
        iframes: [mockIframe(mockSelection('old browser', false))],
        clipboard: {},
    });
    const { copyEmailIframeSelection } = loadCopyFns(env);
    assert.strictEqual(copyEmailIframeSelection(), true);
    assert.deepStrictEqual(env.execCalls, ['copy']);
});

// --- wiring shape ----------------------------------------------------------
// Behavioral tests above prove the copier works; these pin that it is
// actually REACHED — and that the chord guard that stops Ctrl+C from opening
// compose stays at the top of handleNormalModeKey.

function fnBody(src, decl) {
    const start = src.indexOf(decl);
    assert.notStrictEqual(start, -1, `${decl} must exist in app.js`);
    const close = src.indexOf('\n}', start);
    return src.slice(start, close + 2);
}

test('handleKeyDown routes Ctrl/Cmd+C through copyEmailIframeSelection', () => {
    const body = fnBody(APP_JS, 'function handleKeyDown(e)');
    assert.match(body, /copyEmailIframeSelection\(\)/,
        'the Ctrl+C branch must call copyEmailIframeSelection');
    assert.match(body, /e\.key\.toLowerCase\(\) === 'c'/,
        'the branch must key on c');
});

test('handleNormalModeKey ignores Ctrl/Cmd/Alt chords entirely', () => {
    const body = fnBody(APP_JS, 'function handleNormalModeKey(e)');
    const guard = body.indexOf('if (e.ctrlKey || e.metaKey || e.altKey) return;');
    const firstSwitch = body.indexOf('switch (key)');
    assert.notStrictEqual(guard, -1, 'chord guard must exist');
    assert.ok(guard < firstSwitch,
        'chord guard must run before any binding dispatch');
});

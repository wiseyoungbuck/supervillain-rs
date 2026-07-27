// Behavioral tests for the escape primitives (kata hp8w).
//
// Tier-1 (src/routes.rs) pins that the call SITES use escapeHtml; these prove
// the primitive itself neutralizes the attack class. The code-shape tests
// would pass against an escapeHtml that did nothing; these wouldn't.
//
// We extract the REAL escapeHtml from static/app.js, stand up a minimal mock
// DOM that implements the browser's textContent→innerHTML HTML-escaping
// contract (entity-encode & < > and nothing more — exactly what the browser
// serializer does), and assert a real script-tag payload is neutralized.
//
// Run:  node --test tests/escape_test.cjs
// Wired into cargo test via tests/escape_test.rs (shells out; skips — does not
// fail — if node is absent, so CI images without node stay green; the Tier-1
// tests guard the fix regardless of node availability).

const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const APP_JS = fs.readFileSync(
    path.join(__dirname, '..', 'static', 'app.js'),
    'utf8',
);

// Minimal DOM shim: textContent assignment serializes via HTML entity encoding
// for < > & (the browser's textContent→innerHTML contract). innerHTML read
// returns that serialized string. This is the WHOLE behavior escapeHtml relies
// on; we implement exactly that and nothing more.
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

// Extract the real escapeHtml from app.js and eval it with a faked `document`
// injected as a parameter. Mirrors the column-0 brace rule: the function's
// closing brace is the only `}` at column 0 in its body, so the first "\n}"
// after the decl is the close.
function loadEscapeHtml() {
    const start = APP_JS.indexOf('function escapeHtml(');
    assert.notStrictEqual(start, -1, 'function escapeHtml( must exist in app.js');
    const close = APP_JS.indexOf('\n}', start);
    assert.notStrictEqual(close, -1, 'escapeHtml must close with a column-0 brace');
    // close is the index of '\n'; the '}' is at close+1, so slice through
    // close+2 to include the function's closing brace (we eval, so we need it).
    const code = APP_JS.slice(start, close + 2) + '\nreturn { escapeHtml };';
    // eslint-disable-next-line no-new-func
    return new Function('document', code)(makeDocument());
}

test('escapeHtml neutralizes a script-tag payload (hp8w class)', () => {
    const { escapeHtml } = loadEscapeHtml();
    const out = escapeHtml('<img src=x onerror=alert(1)>');
    assert.equal(out, '&lt;img src=x onerror=alert(1)&gt;');
    assert.ok(!out.includes('<img'), 'no live tag survives');
});

test('escapeHtml neutralizes an entity-breaking payload', () => {
    // & must be encoded first or it would form a new entity with the escaping
    // itself (the classic double-escape: escaping < as &lt; then having a raw
    // &amp;lt; would be wrong). The browser encodes & first; the shim must too.
    const { escapeHtml } = loadEscapeHtml();
    const out = escapeHtml('a&b<c>d');
    assert.equal(out, 'a&amp;b&lt;c&gt;d');
});

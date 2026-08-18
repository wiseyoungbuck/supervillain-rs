// Behavioral tests for Share Availability compose insertion (kata mtqp).
//
// Extracts the REAL availabilityText and insertAtCursor from static/app.js
// and asserts the exact text block built from a mocked
// GET /api/calendar/free-slots response — contiguous slots merged into
// ranges, grouped per day — and the exact compose-body value after inserting
// at a cursor position (including replacing a selection).
//
// Run:  node --test tests/share_availability_test.cjs
// Wired into cargo test via tests/share_availability_test.rs.

// Deterministic local-time math: pin the timezone BEFORE any Date exists.
process.env.TZ = 'UTC';

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

function loadHelpers() {
    const code = [
        extractFunction(APP_JS, 'function peekTimeLabel('),
        extractFunction(APP_JS, 'function availabilityText('),
        extractFunction(APP_JS, 'function insertAtCursor('),
    ].join('\n');
    // eslint-disable-next-line no-new-func
    return new Function(code + '\nreturn { availabilityText, insertAtCursor };')();
}

// A /api/calendar/free-slots response: two contiguous morning slots (must
// merge), a separate afternoon slot, and a next-day slot.
const SLOTS = [
    { start: '2026-08-18T09:00:00Z', end: '2026-08-18T09:30:00Z' },
    { start: '2026-08-18T09:30:00Z', end: '2026-08-18T10:00:00Z' },
    { start: '2026-08-18T14:00:00Z', end: '2026-08-18T14:30:00Z' },
    { start: '2026-08-19T11:00:00Z', end: '2026-08-19T11:30:00Z' },
];

const EXPECTED_TEXT = `Would any of these times work?

- Tue, Aug 18: 09:00–10:00, 14:00–14:30
- Wed, Aug 19: 11:00–11:30

(times in UTC)`;

test('availabilityText merges contiguous slots and groups per day, exactly', () => {
    const { availabilityText } = loadHelpers();
    assert.strictEqual(availabilityText(SLOTS, 'UTC'), EXPECTED_TEXT);
});

test('availabilityText with no slots yields empty string (caller shows status)', () => {
    const { availabilityText } = loadHelpers();
    assert.strictEqual(availabilityText([], 'UTC'), '');
});

test('insertAtCursor splices at the caret and reports the new caret', () => {
    const { insertAtCursor } = loadHelpers();
    const out = insertAtCursor('Hi,\n\nBest', 5, 5, 'SLOTS');
    assert.deepStrictEqual(out, { value: 'Hi,\n\nSLOTSBest', caret: 10 });
});

test('insertAtCursor replaces an active selection', () => {
    const { insertAtCursor } = loadHelpers();
    const out = insertAtCursor('see TIMES below', 4, 9, 'my availability');
    assert.deepStrictEqual(out, { value: 'see my availability below', caret: 19 });
});

test('mocked response → exact inserted compose body at the cursor', () => {
    const { availabilityText, insertAtCursor } = loadHelpers();
    const body = 'Hi,\n\n\nThanks!';
    const caret = 5; // between the greeting and the sign-off
    const text = availabilityText(SLOTS, 'UTC');
    const out = insertAtCursor(body, caret, caret, text);
    assert.strictEqual(out.value, `Hi,\n\n${EXPECTED_TEXT}\nThanks!`);
    assert.strictEqual(out.caret, 5 + text.length);
});

// Behavioral tests for the calendar peek render (kata j6e4).
//
// Extracts the REAL calendarPeekHtml (and the helpers it composes with) from
// static/app.js and asserts the rendered day/week HTML for mocked events:
// all-day vs timed placement, day-column assignment, cross-midnight clamping,
// escaping, and the 200-event-week perf budget.
//
// Run:  node --test tests/calendar_peek_test.cjs
// Wired into cargo test via tests/calendar_peek_test.rs (mirrors palette_test).

// Deterministic local-time math: the render helpers use local Date parts,
// so pin the process timezone BEFORE any Date is constructed.
process.env.TZ = 'UTC';

const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const APP_JS = fs.readFileSync(
    path.join(__dirname, '..', 'static', 'app.js'),
    'utf8',
);

// Extract one real column-0 function body from app.js (the js_fn_body
// convention shared with palette_test.cjs and the Rust contract tests).
function extractFunction(src, declaration) {
    const start = src.indexOf(declaration);
    assert.notStrictEqual(start, -1, `${declaration} must exist in app.js`);
    const close = src.indexOf('\n}', start);
    assert.notStrictEqual(close, -1, `${declaration} must close with a column-0 brace`);
    return src.slice(start, close + 2);
}

// escapeHtml relies on the browser's textContent -> innerHTML serialization
// (same shim as palette_test.cjs).
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

// Load the real render pipeline: calendarPeekHtml plus every helper it calls.
function loadCalendarPeekHtml() {
    const code = [
        extractFunction(APP_JS, 'function escapeHtml('),
        extractFunction(APP_JS, 'function escapeAttr('),
        extractFunction(APP_JS, 'function peekDayKey('),
        extractFunction(APP_JS, 'function peekTimeLabel('),
        extractFunction(APP_JS, 'function calendarPeekHtml('),
    ].join('\n');
    // eslint-disable-next-line no-new-func
    return new Function('document', code + '\nreturn calendarPeekHtml;')(makeEscapeDocument());
}

// Events as GET /api/calendar/events emits them (camelCase RangeEvents).
function timed(uid, summary, startIso, endIso) {
    return { uid, summary, start: startIso, end: endIso, allDay: false, location: null, account: 'acct-1' };
}
function allDay(uid, summary, startIso, endIso) {
    return { uid, summary, start: startIso, end: endIso, allDay: true, location: null, account: 'acct-1' };
}

// Anchor: Tue 2026-08-18. Week (Monday-first) runs 2026-08-17..2026-08-23.
const ANCHOR = '2026-08-18';

const EVENTS = [
    timed('t1', 'Standup', '2026-08-18T10:00:00Z', '2026-08-18T11:00:00Z'),
    allDay('ad1', 'Offsite', '2026-08-18T00:00:00Z', '2026-08-19T00:00:00Z'),
    timed('t2', 'Thursday sync', '2026-08-20T09:00:00Z', '2026-08-20T09:30:00Z'),
];

test('day mode renders only the anchor day, timed vs all-day placed apart', () => {
    const calendarPeekHtml = loadCalendarPeekHtml();
    const html = calendarPeekHtml(EVENTS, 'day', ANCHOR);

    // Exactly one day column: the anchor's.
    const dayCols = html.match(/data-day="[0-9-]+"/g) || [];
    assert.deepStrictEqual(dayCols, ['data-day="2026-08-18"']);

    // The timed event sits in the timed lane with its grid position:
    // 10:00 UTC = minute 600, one hour = 60.
    const timedLane = html.split('class="peek-timed"')[1] || '';
    assert.match(timedLane, /data-uid="t1"/);
    assert.match(timedLane, /--peek-start:600;/);
    assert.match(timedLane, /--peek-dur:60;/);
    assert.match(timedLane, /10:00/);
    assert.match(timedLane, /Standup/);

    // The all-day event sits in the all-day strip, not in the timed lane.
    const alldayStrip = html.split('class="peek-allday"')[1].split('class="peek-timed"')[0];
    assert.match(alldayStrip, /data-uid="ad1"/);
    assert.match(alldayStrip, /Offsite/);
    assert.doesNotMatch(timedLane, /data-uid="ad1"/);

    // Thursday's event is outside the day window.
    assert.doesNotMatch(html, /data-uid="t2"/);
});

test('week mode renders Monday-first columns and places each event in its day', () => {
    const calendarPeekHtml = loadCalendarPeekHtml();
    const html = calendarPeekHtml(EVENTS, 'week', ANCHOR);

    const dayCols = (html.match(/data-day="([0-9-]+)"/g) || []).map(m => m.slice(10, -1));
    assert.deepStrictEqual(dayCols, [
        '2026-08-17', '2026-08-18', '2026-08-19', '2026-08-20',
        '2026-08-21', '2026-08-22', '2026-08-23',
    ]);

    // t2 lands in the Thursday column.
    const thursday = html.split('data-day="2026-08-20"')[1].split('data-day="2026-08-21"')[0];
    assert.match(thursday, /data-uid="t2"/);
    // t1 does not leak into Thursday.
    assert.doesNotMatch(thursday, /data-uid="t1"/);
});

test('a multi-day all-day event spans its covered days only', () => {
    const calendarPeekHtml = loadCalendarPeekHtml();
    // All-day 18th..20th exclusive: covers the 18th and 19th, not the 20th.
    const span = [allDay('span1', 'Conference', '2026-08-18T00:00:00Z', '2026-08-20T00:00:00Z')];
    const html = calendarPeekHtml(span, 'week', ANCHOR);
    const day = (key) => html.split(`data-day="${key}"`)[1].split(/data-day="|$/)[0];
    assert.match(day('2026-08-18'), /data-uid="span1"/);
    assert.match(day('2026-08-19'), /data-uid="span1"/);
    assert.doesNotMatch(day('2026-08-20'), /data-uid="span1"/);
});

test('a timed event crossing midnight clamps to its start day', () => {
    const calendarPeekHtml = loadCalendarPeekHtml();
    const cross = [timed('x1', 'Late deploy', '2026-08-18T23:30:00Z', '2026-08-19T01:00:00Z')];
    const html = calendarPeekHtml(cross, 'day', ANCHOR);
    assert.match(html, /data-uid="x1"/);
    assert.match(html, /--peek-start:1410;/);
    // Clamped to midnight: 30 minutes, not 90.
    assert.match(html, /--peek-dur:30;/);
});

test('event summaries render escaped, never as live markup', () => {
    const calendarPeekHtml = loadCalendarPeekHtml();
    const evil = [timed('e1', '<img src=x onerror=alert(1)>', '2026-08-18T10:00:00Z', '2026-08-18T11:00:00Z')];
    const html = calendarPeekHtml(evil, 'day', ANCHOR);
    assert.doesNotMatch(html, /<img/);
    assert.match(html, /&lt;img/);
});

test('perf budget: 200-event week renders under 100ms', () => {
    const calendarPeekHtml = loadCalendarPeekHtml();
    // ~29 events per weekday, spread across working hours.
    const events = Array.from({ length: 200 }, (_, i) => {
        const day = 17 + (i % 7);
        const hour = 8 + (i % 10);
        return timed(
            `perf-${i}`,
            `Perf event ${i}`,
            `2026-08-${String(day).padStart(2, '0')}T${String(hour).padStart(2, '0')}:00:00Z`,
            `2026-08-${String(day).padStart(2, '0')}T${String(hour).padStart(2, '0')}:30:00Z`,
        );
    });
    const started = process.hrtime.bigint();
    const html = calendarPeekHtml(events, 'week', ANCHOR);
    const elapsedMs = Number(process.hrtime.bigint() - started) / 1e6;
    assert.strictEqual((html.match(/class="peek-event/g) || []).length, 200);
    assert.ok(
        elapsedMs < 100,
        `200-event week render took ${elapsedMs.toFixed(1)}ms (budget 100ms)`,
    );
});

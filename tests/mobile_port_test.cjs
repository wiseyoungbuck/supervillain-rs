// Behavioral tests for the desktop→mobile ports in static/mobile/app.js (kata r29v).
//
// The ticket named findAttachments / findCalendarBlobId / markRead /
// sanitizeHtml. None of those identifiers survive in the tree: attachment and
// calendar extraction moved server-side (routes.rs returns a full
// `attachments` array and `calendarEvent`), read-marking is `toggleUnread`,
// and a client-side `sanitizeHtml` was deliberately deleted in favour of the
// script-less iframe sandbox — routes.rs actively asserts it must NOT come
// back. So this suite covers the functions those names now point at:
//
//   findAttachments      → renderAttachments (+ attachmentUrl, getFileIcon,
//                          formatFileSize)
//   findCalendarBlobId   → renderCalendarCard
//   markRead             → toggleUnread
//   sanitizeHtml         → escapeHtml / escapeAttr / segmentUrls /
//                          linkifyText / wrapEmailHtml — the escaping layer
//                          that is mobile's actual injection defence now that
//                          the sanitizer is gone.
//
// Every test extracts the REAL function from the shipped bundle and evals it
// against fixtures (the tests/email_refresh_test.cjs and invite_chip_test.cjs
// idiom); nothing here re-implements renderer logic. Functions that are
// verbatim ports are additionally pinned byte-for-byte in routes.rs, so the
// two bundles cannot drift silently; the ones covered here are the divergent
// copies, which a pin cannot protect.
//
// linkifyHtml and htmlToPlainText are absent by necessity: both go through
// DOMParser, which Node does not provide, and faking it would test the fake.
// segmentUrls — the URL detection linkifyHtml delegates to — carries that
// coverage instead.
//
// Run:  node --test tests/mobile_port_test.cjs
// Wired into cargo test via tests/mobile_port_test.rs.

const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const MOBILE = fs.readFileSync(
    path.join(__dirname, '..', 'static', 'mobile', 'app.js'),
    'utf8',
);

// Mirrors the Rust js_fn_body helper's column-0 brace rule: the declaration's
// closing brace is the only `}` at column 0 in the region. We eval rather than
// substring-match, so the slice keeps the brace.
function extractFunction(src, declaration) {
    const start = src.indexOf(declaration);
    assert.notStrictEqual(start, -1, `${declaration} must exist`);
    const close = src.indexOf('\n}', start);
    assert.notStrictEqual(close, -1, `${declaration} must close with a column-0 brace`);
    return src.slice(start, close + 2);
}

// Pull a single-statement `const NAME = ...;` out of the bundle and eval it.
// Data collaborators come from the shipped source for the same reason the
// functions do: a restated copy in the test keeps passing after the real one
// is reworded, which is the drift this suite exists to catch.
function extractConst(src, name) {
    const decl = `const ${name} = `;
    const start = src.indexOf(decl);
    assert.notStrictEqual(start, -1, `${name} must exist`);
    const end = src.indexOf(';\n', start);
    assert.notStrictEqual(end, -1, `${name} must terminate`);
    // eslint-disable-next-line no-new-func
    return new Function(`${src.slice(start, end + 1)}\nreturn ${name};`)();
}

// Extract one or more declarations and hand back the last one named, with any
// collaborators injected as parameters rather than globals (the
// email_iframe_test.cjs convention — Node's own globals stay untouched).
function loadMobile(declarations, exported, injected = {}) {
    const code = declarations.map(d => extractFunction(MOBILE, d)).join('\n')
        + `\nreturn ${exported};`;
    const names = Object.keys(injected);
    // eslint-disable-next-line no-new-func
    return new Function(...names, code)(...names.map(n => injected[n]));
}

// ---------------------------------------------------------------------------
// escapeHtml / escapeAttr — the injection defence that replaced sanitizeHtml
// ---------------------------------------------------------------------------

function loadEscapeHtml() {
    return loadMobile(['function escapeHtml('], 'escapeHtml');
}

function loadEscapeAttr() {
    return loadMobile(
        ['function escapeHtml(', 'function escapeAttr('],
        'escapeAttr',
    );
}

test('r29v: mobile escapeHtml neutralizes tag and entity syntax', () => {
    const escapeHtml = loadEscapeHtml();
    assert.equal(
        escapeHtml('<script>alert(1)</script>'),
        '&lt;script&gt;alert(1)&lt;/script&gt;',
    );
    assert.equal(
        escapeHtml('<img src=x onerror=alert(1)>'),
        '&lt;img src=x onerror=alert(1)&gt;',
    );
    // The ampersand must go first, or the escapes themselves become
    // double-decodable ("&amp;lt;" round-tripping back to "<").
    assert.equal(escapeHtml('&lt;b&gt;'), '&amp;lt;b&amp;gt;');
    assert.equal(escapeHtml('a & b'), 'a &amp; b');
});

// Mobile's escapeHtml is a regex chain; desktop's is textContent→innerHTML,
// which does NOT escape quotes. That divergence is load-bearing rather than
// accidental: mobile interpolates escapeHtml output straight into quoted
// attributes (renderAttachments' href/src, linkifyText's href), where the
// desktop implementation would let an attacker close the attribute.
test('r29v: mobile escapeHtml escapes double quotes, unlike the desktop copy', () => {
    const escapeHtml = loadEscapeHtml();
    assert.equal(escapeHtml('say "hi"'), 'say &quot;hi&quot;');
    assert.doesNotMatch(
        escapeHtml('" onload="alert(1)'),
        /"/,
        'a quote surviving escapeHtml breaks out of every attribute mobile builds',
    );
});

test('r29v: mobile escapeAttr additionally escapes single quotes', () => {
    const escapeAttr = loadEscapeAttr();
    assert.equal(escapeAttr("it's"), 'it&#39;s');
    assert.equal(
        escapeAttr(`' onerror='alert(1)`),
        '&#39; onerror=&#39;alert(1)',
    );
    // Everything escapeHtml handles must still be handled.
    assert.equal(escapeAttr('<a href="x">&'), '&lt;a href=&quot;x&quot;&gt;&amp;');
});

// ---------------------------------------------------------------------------
// segmentUrls / linkifyText — the URL parsing linkifyHtml delegates to
// ---------------------------------------------------------------------------

function loadSegmentUrls() {
    return loadMobile(['function segmentUrls('], 'segmentUrls');
}

function loadLinkifyText() {
    return loadMobile(
        ['function escapeHtml(', 'function segmentUrls(', 'function linkifyText('],
        'linkifyText',
    );
}

test('r29v: mobile segmentUrls splits text around http(s) URLs', () => {
    const segmentUrls = loadSegmentUrls();
    assert.deepEqual(segmentUrls('no links here'), [{ text: 'no links here' }]);
    assert.deepEqual(segmentUrls('see https://example.com/a now'), [
        { text: 'see ' },
        { text: 'https://example.com/a', url: 'https://example.com/a' },
        { text: ' now' },
    ]);
    assert.deepEqual(segmentUrls('http://a.test https://b.test'), [
        { text: 'http://a.test', url: 'http://a.test' },
        { text: ' ' },
        { text: 'https://b.test', url: 'https://b.test' },
    ]);
});

test('r29v: mobile segmentUrls leaves sentence punctuation outside the link', () => {
    const segmentUrls = loadSegmentUrls();
    // Trailing punctuation belongs to the prose, not the URL — swallowing it
    // produces a 404 on click.
    assert.deepEqual(segmentUrls('go to https://example.com/page.'), [
        { text: 'go to ' },
        { text: 'https://example.com/page', url: 'https://example.com/page' },
        { text: '.' },
    ]);
    assert.deepEqual(segmentUrls('really? https://example.com/x!?'), [
        { text: 'really? ' },
        { text: 'https://example.com/x', url: 'https://example.com/x' },
        { text: '!?' },
    ]);
    // ...but path punctuation that is not trailing survives.
    assert.deepEqual(segmentUrls('https://example.com/a.b/c'), [
        { text: 'https://example.com/a.b/c', url: 'https://example.com/a.b/c' },
    ]);
});

test('r29v: mobile segmentUrls stops URLs at markup and quote delimiters', () => {
    const segmentUrls = loadSegmentUrls();
    // These characters end a URL so a bare link inside markup or quotes
    // cannot swallow the surrounding syntax.
    for (const [input, url] of [
        ['https://example.com/a<b', 'https://example.com/a'],
        ['https://example.com/a>b', 'https://example.com/a'],
        ['https://example.com/a"b', 'https://example.com/a'],
        ["https://example.com/a'b", 'https://example.com/a'],
        ['(https://example.com/a)', 'https://example.com/a'],
        ['[https://example.com/a]', 'https://example.com/a'],
    ]) {
        const segments = segmentUrls(input);
        const linked = segments.filter(s => s.url);
        assert.equal(linked.length, 1, `${input} must yield exactly one link`);
        assert.equal(linked[0].url, url, `${input} must stop the URL at the delimiter`);
        // Nothing may be lost or duplicated in the split.
        assert.equal(segments.map(s => s.text).join(''), input);
    }
});

test('r29v: mobile segmentUrls does not linkify non-http schemes', () => {
    const segmentUrls = loadSegmentUrls();
    for (const text of [
        'javascript:alert(1)',
        'data:text/html,<script>alert(1)</script>',
        'file:///etc/passwd',
        'mailto:a@b.test',
    ]) {
        assert.deepEqual(
            segmentUrls(text).filter(s => s.url),
            [],
            `${text} must not become a link`,
        );
    }
});

test('r29v: mobile linkifyText escapes both the anchor and the prose around it', () => {
    const linkifyText = loadLinkifyText();
    assert.equal(
        linkifyText('hi <b>there</b>'),
        'hi &lt;b&gt;there&lt;/b&gt;',
        'text with no URL must still be fully escaped',
    );
    const html = linkifyText('see https://example.com/a <script>x</script>');
    assert.match(
        html,
        /<a href="https:\/\/example\.com\/a" target="_blank" rel="noopener noreferrer">https:\/\/example\.com\/a<\/a>/,
    );
    assert.match(html, /&lt;script&gt;x&lt;\/script&gt;/);
    assert.doesNotMatch(html, /<script/, 'no attacker tag may survive linkifyText');
});

test('r29v: mobile linkifyText cannot be broken out of its href attribute', () => {
    const linkifyText = loadLinkifyText();
    // The URL regex stops at `"`, so the quote lands in the escaped tail; both
    // halves of that contract have to hold for the anchor to stay intact.
    const html = linkifyText('https://example.com/a" onmouseover="alert(1)');
    assert.match(html, /href="https:\/\/example\.com\/a"/);
    assert.doesNotMatch(html, /onmouseover="alert/, 'the injected handler must be inert');
    assert.match(html, /&quot;/);
});

// ---------------------------------------------------------------------------
// wrapEmailHtml — the iframe srcdoc envelope
// ---------------------------------------------------------------------------

test('r29v: mobile wrapEmailHtml frames sender HTML without altering it', () => {
    const wrapEmailHtml = loadMobile(['function wrapEmailHtml('], 'wrapEmailHtml');
    const body = '<p>hello</p><script>alert(1)</script>';
    const doc = wrapEmailHtml(body);
    // The sandbox is the security boundary; this function deliberately does
    // NOT sanitize, and must not start pretending to (that was sanitizeHtml's
    // job, and it was removed on purpose).
    assert.ok(doc.includes(body), 'sender HTML must be embedded verbatim');
    assert.ok(doc.startsWith('<!doctype html>'));
    assert.ok(doc.endsWith('</body></html>'));
});

test('r29v: mobile wrapEmailHtml pins a light canvas and new-tab links', () => {
    const wrapEmailHtml = loadMobile(['function wrapEmailHtml('], 'wrapEmailHtml');
    const doc = wrapEmailHtml('');
    // Senders author against white with explicit dark text and no background;
    // a dark canvas renders those messages unreadable (kata tgax).
    assert.match(doc, /<meta name="color-scheme" content="light">/);
    assert.match(doc, /background:#fff/);
    // <base target="_blank"> is what makes links leave the script-less iframe
    // instead of navigating it.
    assert.match(doc, /<base target="_blank">/);
});

// ---------------------------------------------------------------------------
// Attachment rendering (the findAttachments surface)
// ---------------------------------------------------------------------------

function loadRenderAttachments(accountId = 'acct-1') {
    return loadMobile(
        [
            'function escapeHtml(',
            'function formatFileSize(',
            'function getFileIcon(',
            'function attachmentUrl(',
            'function renderAttachments(',
        ],
        'renderAttachments',
        { state: { currentAccount: { id: accountId } } },
    );
}

function attachment(overrides = {}) {
    return {
        name: 'report.pdf',
        mime_type: 'application/pdf',
        size: 2048,
        blob_id: 'blob-1',
        ...overrides,
    };
}

test('r29v: mobile formatFileSize scales units and keeps whole bytes exact', () => {
    const formatFileSize = loadMobile(['function formatFileSize('], 'formatFileSize');
    assert.equal(formatFileSize(0), '0 B');
    assert.equal(formatFileSize(-1), '0 B');
    assert.equal(formatFileSize(512), '512 B');
    assert.equal(formatFileSize(1024), '1.0 KB');
    assert.equal(formatFileSize(1536), '1.5 KB');
    assert.equal(formatFileSize(1024 * 1024), '1.0 MB');
    assert.equal(formatFileSize(3 * 1024 * 1024 * 1024), '3.0 GB');
});

test('r29v: mobile getFileIcon prefers MIME type and falls back to extension', () => {
    const getFileIcon = loadMobile(['function getFileIcon('], 'getFileIcon');
    assert.equal(getFileIcon('image/png', 'a.png'), '\u{1F5BC}');
    assert.equal(getFileIcon('application/pdf', 'a.bin'), '\u{1F4C4}');
    assert.equal(getFileIcon('application/octet-stream', 'a.pdf'), '\u{1F4C4}');
    assert.equal(getFileIcon('audio/mpeg', 'a.mp3'), '\u{1F3B5}');
    assert.equal(getFileIcon('video/mp4', 'a.mp4'), '\u{1F3AC}');
    assert.equal(getFileIcon('application/octet-stream', 'a.ZIP'), '\u{1F4E6}');
    assert.equal(getFileIcon('application/octet-stream', 'a.xlsx'), '\u{1F4CA}');
    assert.equal(getFileIcon('application/octet-stream', 'a.docx'), '\u{1F4DD}');
    assert.equal(getFileIcon('application/octet-stream', 'notes.txt'), '\u{1F4C3}');
    // Unknown type, and a name with no extension at all.
    assert.equal(getFileIcon('application/octet-stream', 'a.xyz'), '\u{1F4CE}');
    assert.equal(getFileIcon('application/octet-stream', 'README'), '\u{1F4CE}');
});

test('r29v: mobile attachmentUrl percent-encodes every path segment', () => {
    const attachmentUrl = loadMobile(
        ['function attachmentUrl('],
        'attachmentUrl',
        { state: { currentAccount: { id: 'acct/1' } } },
    );
    const url = attachmentUrl('id/with?chars', attachment({
        blob_id: 'blob/2',
        name: 'my report.pdf',
    }));
    assert.equal(
        url,
        '/api/emails/id%2Fwith%3Fchars/attachments/blob%2F2/my%20report.pdf?account=acct%2F1',
    );
    // A raw slash in any segment would re-route the request to a different
    // email, blob, or account.
    assert.doesNotMatch(url.slice('/api/emails/'.length).split('?')[0], /^[^/]*\/[^/]*\/[^/]*\/[^/]*\//);
});

test('r29v: mobile renderAttachments lists every attachment with icon, size and link', () => {
    const renderAttachments = loadRenderAttachments();
    const html = renderAttachments(
        [
            attachment({ name: 'a.pdf', blob_id: 'b1', size: 2048 }),
            attachment({ name: 'b.txt', mime_type: 'text/plain', blob_id: 'b2', size: 10 }),
        ],
        'email-1',
    );
    assert.match(html, /Attachments \(2\)/);
    assert.match(html, /href="\/api\/emails\/email-1\/attachments\/b1\/a\.pdf\?account=acct-1"/);
    assert.match(html, /href="\/api\/emails\/email-1\/attachments\/b2\/b\.txt\?account=acct-1"/);
    assert.match(html, /<span class="att-name">a\.pdf<\/span>/);
    assert.match(html, /<span class="att-size">2\.0 KB<\/span>/);
    assert.match(html, /<span class="att-size">10 B<\/span>/);
    assert.equal((html.match(/class="att-item"/g) || []).length, 2);
    // Every link must be safe to open cross-origin from the detail view.
    assert.equal((html.match(/rel="noopener noreferrer"/g) || []).length, 2);
});

test('r29v: mobile renderAttachments shows Download All only for 2+ attachments', () => {
    const renderAttachments = loadRenderAttachments();
    // kata 0g9v: one attachment already has its own row; a second control is
    // noise until there is more than one thing to download.
    assert.doesNotMatch(renderAttachments([], 'e'), /att-download-all/);
    assert.doesNotMatch(renderAttachments([attachment()], 'e'), /att-download-all/);
    assert.match(
        renderAttachments([attachment(), attachment({ blob_id: 'b2' })], 'e'),
        /att-download-all/,
    );
    assert.match(renderAttachments([], 'e'), /Attachments \(0\)/);
});

test('r29v: mobile renderAttachments inlines a preview for images only', () => {
    const renderAttachments = loadRenderAttachments();
    const image = renderAttachments(
        [attachment({ name: 'p.png', mime_type: 'image/png' })],
        'e',
    );
    assert.match(image, /<img class="att-preview" loading="lazy" src="[^"]+" alt="">/);
    for (const mime of ['application/pdf', 'text/plain', 'application/octet-stream']) {
        assert.doesNotMatch(
            renderAttachments([attachment({ mime_type: mime })], 'e'),
            /att-preview/,
            `${mime} must not get an inline preview`,
        );
    }
});

test('r29v: mobile renderAttachments escapes attacker-controlled filenames', () => {
    const renderAttachments = loadRenderAttachments();
    // Filename and blob id come off the wire from the sender's message.
    const html = renderAttachments(
        [attachment({
            name: '"><img src=x onerror=alert(1)>.pdf',
            blob_id: '"><script>alert(2)</script>',
        })],
        '"><b>',
    );
    // The payload may appear as inert escaped text, but never as a live tag:
    // no `<` from sender input may reach the output unescaped.
    assert.doesNotMatch(html, /<script|<img(?! class="att-preview")/, 'no injected tag may survive');
    assert.match(html, /&lt;img src=x onerror=alert\(1\)&gt;/);
    // The blob id reaches the output only inside the URL, percent-encoded.
    assert.match(html, /attachments\/%22%3E%3Cscript%3E/);
    // Nor may a quote close an attribute early: every att-item href must still
    // be a single well-formed attribute.
    assert.match(html, /<a class="att-item" href="[^"]*" target="_blank" rel="noopener noreferrer">/);
    assert.match(html, /&quot;/);
});

// ---------------------------------------------------------------------------
// Calendar card (the findCalendarBlobId surface)
// ---------------------------------------------------------------------------

// formatEventTimeRange is injected as a fixed stub: it goes through
// toLocaleString, so asserting on its real output would pin the runner's
// locale and timezone rather than the card's structure.
function loadRenderCalendarCard(timeRange = 'Mon, Jan 5, 9:00 AM') {
    return loadMobile(
        ['function escapeHtml(', 'function renderCalendarCard('],
        'renderCalendarCard',
        {
            RSVP_LABELS: extractConst(MOBILE, 'RSVP_LABELS'),
            formatEventTimeRange: () => timeRange,
        },
    );
}

function calendarEvent(overrides = {}) {
    return {
        method: 'REQUEST',
        summary: 'Standup',
        dtstart: '2026-01-05T09:00:00Z',
        dtend: '2026-01-05T09:30:00Z',
        ...overrides,
    };
}

test('r29v: mobile renderCalendarCard offers RSVP actions only for REQUEST invites', () => {
    const renderCalendarCard = loadRenderCalendarCard();
    const request = renderCalendarCard(calendarEvent());
    for (const status of ['ACCEPTED', 'TENTATIVE', 'DECLINED']) {
        assert.match(request, new RegExp(`data-status="${status}"`));
    }
    // A REPLY or CANCEL carries no invitation to answer.
    for (const method of ['REPLY', 'CANCEL', 'PUBLISH', undefined]) {
        assert.doesNotMatch(
            renderCalendarCard(calendarEvent({ method })),
            /rsvp-btn/,
            `METHOD:${method} must not render RSVP buttons`,
        );
    }
});

test('r29v: mobile renderCalendarCard highlights the status the user chose', () => {
    const renderCalendarCard = loadRenderCalendarCard();
    for (const [status, cls] of [
        ['ACCEPTED', 'accept'],
        ['TENTATIVE', 'maybe'],
        ['DECLINED', 'decline'],
    ]) {
        const html = renderCalendarCard(calendarEvent({ user_rsvp_status: status }));
        assert.match(html, new RegExp(`class="rsvp-btn ${cls} active"`));
        assert.equal(
            (html.match(/ active"/g) || []).length,
            1,
            'exactly one RSVP button may be active',
        );
    }
    // NEEDS-ACTION is "not answered yet", not an answer.
    const pending = renderCalendarCard(calendarEvent({ user_rsvp_status: 'NEEDS-ACTION' }));
    assert.doesNotMatch(pending, / active"/);
    assert.doesNotMatch(pending, /You responded/);
});

test('r29v: mobile renderCalendarCard labels an answered invite', () => {
    const renderCalendarCard = loadRenderCalendarCard();
    assert.match(
        renderCalendarCard(calendarEvent({ user_rsvp_status: 'TENTATIVE' })),
        /<div class="rsvp-status-label">You responded Maybe<\/div>/,
    );
    assert.doesNotMatch(renderCalendarCard(calendarEvent()), /You responded/);
    // An unknown status string must not render "You responded undefined".
    assert.doesNotMatch(
        renderCalendarCard(calendarEvent({ user_rsvp_status: 'BOGUS' })),
        /You responded/,
    );
});

test('r29v: mobile renderCalendarCard banners cancellations and updates exclusively', () => {
    const renderCalendarCard = loadRenderCalendarCard();
    const cancelled = renderCalendarCard(calendarEvent({ method: 'CANCEL' }));
    assert.match(cancelled, /<div class="cal-cancelled">CANCELLED<\/div>/);
    assert.match(cancelled, /class="calendar-card cancelled"/);
    assert.doesNotMatch(cancelled, /cal-updated/);

    const updated = renderCalendarCard(calendarEvent({ isUpdate: true }));
    assert.match(updated, /<div class="cal-updated">Updated — please respond again<\/div>/);
    assert.doesNotMatch(updated, /cal-cancelled/);

    // A cancellation of a rescheduled invite is cancelled, full stop. Note
    // that the exclusivity is carried by the outer `cancelled ?` ternary, not
    // by the `&& !cancelled` in isUpdate — that conjunct is redundant today,
    // so this asserts the observable outcome rather than either guard.
    const both = renderCalendarCard(calendarEvent({ method: 'CANCEL', isUpdate: true }));
    assert.match(both, /cal-cancelled/);
    assert.doesNotMatch(both, /cal-updated/);

    assert.doesNotMatch(renderCalendarCard(calendarEvent()), /cal-cancelled|cal-updated/);
});

test('r29v: mobile renderCalendarCard renders optional fields only when present', () => {
    const renderCalendarCard = loadRenderCalendarCard();
    const bare = renderCalendarCard(calendarEvent());
    assert.doesNotMatch(bare, /cal-location|cal-organizer|cal-attendee-count/);
    assert.match(bare, /<span class="cal-title">Standup<\/span>/);
    // No summary at all still needs a title.
    assert.match(
        renderCalendarCard(calendarEvent({ summary: '' })),
        /<span class="cal-title">Calendar Event<\/span>/,
    );

    const full = renderCalendarCard(calendarEvent({
        location: 'Room 3',
        organizer_email: 'a@b.test',
        attendees: [{}, {}],
    }));
    assert.match(full, /<div class="cal-location">Room 3<\/div>/);
    assert.match(full, /<div class="cal-organizer">a@b\.test<\/div>/);
    assert.match(full, /<div class="cal-attendee-count">2 attendees<\/div>/);

    // Organizer name wins over the address when both are present.
    assert.match(
        renderCalendarCard(calendarEvent({
            organizer_name: 'Ada',
            organizer_email: 'a@b.test',
        })),
        /<div class="cal-organizer">Ada<\/div>/,
    );
    assert.match(
        renderCalendarCard(calendarEvent({ attendees: [{}] })),
        /<div class="cal-attendee-count">1 attendee<\/div>/,
    );
});

test('r29v: mobile renderCalendarCard escapes attacker-controlled ICS fields', () => {
    const renderCalendarCard = loadRenderCalendarCard('<script>alert(4)</script>');
    const html = renderCalendarCard(calendarEvent({
        summary: '<script>alert(1)</script>',
        location: '"><img src=x onerror=alert(2)>',
        organizer_name: '<b>spoofed</b>',
    }));
    // The payload may appear as inert escaped text, but never as a live tag.
    assert.doesNotMatch(html, /<script|<img/, 'no injected tag may survive');
    assert.doesNotMatch(html, /<b>spoofed/, 'organizer must not be able to style itself');
    assert.match(html, /&lt;script&gt;alert\(1\)&lt;\/script&gt;/);
    assert.match(html, /&quot;&gt;&lt;img src=x onerror=alert\(2\)&gt;/);
    // The formatted time range is escaped too — it is derived from ICS input.
    assert.match(html, /&lt;script&gt;alert\(4\)&lt;\/script&gt;/);
});

// ---------------------------------------------------------------------------
// toggleUnread (the markRead surface)
// ---------------------------------------------------------------------------

function loadToggleUnread({ emails = [], cache = {}, screen = 'list', currentEmailId = null, api } = {}) {
    const calls = [];
    const errors = [];
    const renders = [];
    const state = {
        emails,
        emailCache: cache,
        screen,
        currentEmailId,
        api: async (method, path) => {
            calls.push({ method, path });
            if (api) return api(method, path);
            return {};
        },
    };
    const toggleUnread = loadMobile(
        ['async function toggleUnread('],
        'toggleUnread',
        {
            state,
            Screen: { LIST: 'list', DETAIL: 'detail', COMPOSE: 'compose' },
            renderEmailList: () => renders.push('list'),
            renderDetailActionBar: (email) => renders.push({ detail: email && email.id }),
            showError: (context, err) => errors.push({ context, message: err.message }),
        },
    );
    return { toggleUnread, state, calls, errors, renders };
}

test('r29v: mobile toggleUnread posts mark-read for an unread email and flips it', async () => {
    const email = { id: 'e1', isUnread: true };
    const h = loadToggleUnread({ emails: [email] });

    await h.toggleUnread('e1');

    assert.deepEqual(h.calls, [{ method: 'POST', path: '/emails/e1/mark-read' }]);
    assert.equal(email.isUnread, false);
    assert.deepEqual(h.errors, []);
});

test('r29v: mobile toggleUnread posts mark-unread for an already-read email', async () => {
    const email = { id: 'e1', isUnread: false };
    const h = loadToggleUnread({ emails: [email] });

    await h.toggleUnread('e1');

    assert.deepEqual(h.calls, [{ method: 'POST', path: '/emails/e1/mark-unread' }]);
    assert.equal(email.isUnread, true);
});

test('r29v: mobile toggleUnread flips the list row and the cached copy together', async () => {
    // The same email is held twice — as a list row and as a detail-view cache
    // entry. Flipping one and not the other is how the row and the open
    // message end up disagreeing about read state.
    const email = { id: 'e1', isUnread: true };
    const cached = { id: 'e1', isUnread: true };
    const h = loadToggleUnread({ emails: [email], cache: { e1: cached } });

    await h.toggleUnread('e1');

    assert.equal(email.isUnread, false);
    assert.equal(cached.isUnread, false);
});

test('r29v: mobile toggleUnread flips a cache-only email with no list row', async () => {
    // Opening an email deep-linked from a notification leaves it cached but
    // absent from state.emails.
    const cached = { id: 'e1', isUnread: true };
    const h = loadToggleUnread({ cache: { e1: cached }, screen: 'detail', currentEmailId: 'e1' });

    await h.toggleUnread('e1');

    assert.deepEqual(h.calls, [{ method: 'POST', path: '/emails/e1/mark-read' }]);
    assert.equal(cached.isUnread, false);
});

test('r29v: mobile toggleUnread reverts both copies when the request fails', async () => {
    const email = { id: 'e1', isUnread: true };
    const cached = { id: 'e1', isUnread: true };
    const h = loadToggleUnread({
        emails: [email],
        cache: { e1: cached },
        api: () => { throw new Error('offline'); },
    });

    await h.toggleUnread('e1');

    assert.equal(email.isUnread, true, 'a failed request must leave read state where it started');
    assert.equal(cached.isUnread, true);
    assert.deepEqual(h.errors, [{ context: 'Toggle read status', message: 'offline' }]);
});

test('r29v: mobile toggleUnread redraws the surface the user is looking at', async () => {
    const email = { id: 'e1', isUnread: true };
    const list = loadToggleUnread({ emails: [email], screen: 'list' });
    await list.toggleUnread('e1');
    assert.deepEqual(list.renders, ['list']);

    const open = { id: 'e2', isUnread: true };
    const detail = loadToggleUnread({
        emails: [open],
        screen: 'detail',
        currentEmailId: 'e2',
    });
    await detail.toggleUnread('e2');
    assert.deepEqual(detail.renders, [{ detail: 'e2' }]);

    // Toggling from a swipe while a DIFFERENT email is open must not repaint
    // that email's action bar with this email's state.
    const other = { id: 'e3', isUnread: true };
    const elsewhere = loadToggleUnread({
        emails: [other],
        screen: 'detail',
        currentEmailId: 'e2',
    });
    await elsewhere.toggleUnread('e3');
    assert.deepEqual(elsewhere.renders, []);
});

test('r29v: mobile toggleUnread ignores an email it does not hold', async () => {
    const h = loadToggleUnread({ emails: [{ id: 'e1', isUnread: true }] });

    await h.toggleUnread('missing');

    assert.deepEqual(h.calls, [], 'no request may be sent for an unknown email');
    assert.deepEqual(h.renders, []);
});

test('r29v: mobile toggleUnread percent-encodes the email id into the path', async () => {
    // JMAP ids are opaque; a raw slash or `?` would re-route the POST.
    const h = loadToggleUnread({ emails: [{ id: 'a/b?c', isUnread: true }] });

    await h.toggleUnread('a/b?c');

    assert.deepEqual(h.calls, [{ method: 'POST', path: '/emails/a%2Fb%3Fc/mark-read' }]);
});

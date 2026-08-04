// Behavioral tests for the desktop email-iframe content-sizing (kata ceph).
//
// The repo's other JS tests (src/routes.rs #[cfg(test)]) are string-invariant:
// they pin code shapes because there was no JS harness. Node is available on
// this machine, so these tests do better — they extract the REAL
// `sizeIframeToContent` from static/app.js, stand up a mock DOM, and assert
// the runtime behavior:
//
//   A late-loading image must grow the iframe to the full content height EVEN
//   WHEN the email's body is height-pinned by sender CSS (html,body{height:
//   100%}, common in email templates). A pinned body defeats the
//   ResizeObserver — it watches body's border-box, which never changes under
//   height:100% — so a late image that grows body.scrollHeight but not the
//   border-box is never detected, and the iframe stays clipped at the "top
//   ~10%" (the remaining ceph gap).
//
// Run:  node --test tests/email_iframe_test.cjs
// Wired into cargo test via tests/email_iframe_test.rs (mirrors scripts_test).

const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const APP_JS = fs.readFileSync(
    path.join(__dirname, '..', 'static', 'app.js'),
    'utf8',
);

// Extract `const EMAIL_IFRAME_MAX_HEIGHT ... function sizeIframeToContent { ... }`
// from app.js. Mirrors the Rust `js_fn_body` helper's column-0 brace rule: the
// function's closing brace is the only `}` at column 0 in the region (every
// inner closer is indented), so the first "\n}" after the decl is the close.
function extractSizeIframeToContent(src) {
    const constStart = src.indexOf('const EMAIL_IFRAME_MAX_HEIGHT');
    assert.notStrictEqual(constStart, -1, 'EMAIL_IFRAME_MAX_HEIGHT must exist in app.js');
    const fnStart = src.indexOf('function sizeIframeToContent', constStart);
    assert.notStrictEqual(fnStart, -1, 'function sizeIframeToContent must exist in app.js');
    const close = src.indexOf('\n}', fnStart);
    assert.notStrictEqual(close, -1, 'sizeIframeToContent must close with a column-0 brace');
    // `close` is the index of '\n'; the '}' is at close+1, so slice through close+2
    // to include the function's closing brace (the Rust js_fn_body helper drops it
    // because it only matches substrings; we eval, so we need it).
    return src.slice(constStart, close + 2);
}

// Load the real function with faked browser globals injected as PARAMETERS
// (not globalThis) so Node's own timers / test runner are untouched. The
// returned function closes over the consts and these injected globals.
function loadSizeIframeToContent({ requestAnimationFrame, setTimeout, clearTimeout, ResizeObserver }) {
    const code = extractSizeIframeToContent(APP_JS);
    // eslint-disable-next-line no-new-func
    return new Function(
        'ResizeObserver',
        'requestAnimationFrame',
        'setTimeout',
        'clearTimeout',
        code + '\nreturn sizeIframeToContent;',
    )(ResizeObserver, requestAnimationFrame, setTimeout, clearTimeout);
}

function curH(iframe) {
    return parseFloat(iframe.style.height) || 0;
}

function makeImg() {
    const listeners = {};
    return {
        addEventListener(type, fn /*, options */) {
            (listeners[type] ||= []).push(fn);
        },
        removeEventListener() {},
        fire(type) {
            for (const fn of listeners[type] || []) fn();
        },
    };
}

// A mock iframe document. `bodyBorderBox`:
//   'pinned'  — border-box tracks the iframe viewport (= current style.height),
//               simulating sender CSS body{height:100%}. The ResizeObserver
//               watches this, so it never fires when content grows.
//   'content' — border-box tracks content height (body{height:auto}); the RO
//               fires normally. Used by the control test.
function makeMockIframe({ partialHeight, fullHeight, imgs, bodyBorderBox }) {
    const state = { imageLoaded: false };
    const iframe = { style: { height: '0px' } };
    const cur = () => curH(iframe);
    const contentHeight = () => (state.imageLoaded ? fullHeight : partialHeight);
    const bodyRect = () => (bodyBorderBox === 'pinned' ? cur() : contentHeight());
    const body = {
        get scrollHeight() { return contentHeight(); },
        getBoundingClientRect: () => ({ height: bodyRect() }),
    };
    const root = {
        // standards mode: root.scrollHeight = max(viewport, content); here both
        // equal content, so it tracks contentHeight.
        get scrollHeight() { return contentHeight(); },
        getBoundingClientRect: () => ({ height: bodyRect() }),
    };
    iframe.contentDocument = {
        body,
        documentElement: root,
        fonts: { ready: new Promise(() => {}) }, // never resolves — font path not under test
        querySelectorAll: (sel) => (sel === 'img' ? imgs : []),
    };
    return { iframe, state };
}

function fakeTimers() {
    const queue = [];
    return {
        requestAnimationFrame: (fn) => { queue.push(fn); return 1; },
        setTimeout: (fn) => { queue.push(fn); return 1; },
        clearTimeout: () => {},
        flush: () => { while (queue.length) queue.shift()(); },
    };
}

// A ResizeObserver that fires the callback only for observed targets whose
// border-box actually changed — i.e. it models the browser's real delivery
// semantics, which is the whole point (a pinned body never changes).
function mockResizeObserver(instances) {
    return class {
        constructor(cb) { this.cb = cb; this.last = new Map(); instances.push(this); }
        observe(t) { this.last.set(t, t.getBoundingClientRect().height); }
        unobserve(t) { this.last.delete(t); }
        disconnect() { this.last.clear(); }
        tick() {
            for (const [t, last] of this.last) {
                const now = t.getBoundingClientRect().height;
                if (now !== last) { this.last.set(t, now); this.cb(); }
            }
        }
    };
}

// The core regression: a late-loading image must grow the iframe to full
// content height even when the body border-box is pinned (html,body{height:
// 100%}), which is exactly the case the ResizeObserver cannot detect.
test('ceph: late image grows the iframe to full height even when body border-box is pinned', () => {
    const timers = fakeTimers();
    const roInstances = [];
    const sizeIframeToContent = loadSizeIframeToContent({
        ...timers,
        ResizeObserver: mockResizeObserver(roInstances),
    });

    const img = makeImg();
    const { iframe, state } = makeMockIframe({
        partialHeight: 120, // the "top ~10%" measured before the image loads
        fullHeight: 1200,
        imgs: [img],
        bodyBorderBox: 'pinned',
    });

    sizeIframeToContent(iframe);
    timers.flush(); // burst: immediate + rAF + setTimeout(0)

    // The load burst measured only the partial height — the image contributes
    // 0 height until it loads — so the iframe is clipped at the "top 10%".
    assert.equal(curH(iframe), 120);

    // A ResizeObserver delivery does NOT rescue this: body's border-box is
    // pinned to the viewport, so even though body.scrollHeight grew, the RO
    // never fires.
    for (const ro of roInstances) ro.tick();
    assert.equal(curH(iframe), 120, 'RO must not rescue the pinned-body case');

    // The late image finishes loading — body.scrollHeight grows to full height.
    state.imageLoaded = true;
    img.fire('load'); // the fix's image-load listener must re-measure here
    timers.flush();

    assert.equal(
        curH(iframe),
        1200,
        'a late-loading image must grow the iframe to full content height even when the body border-box is pinned (ceph)',
    );
});

// w5ba: sizeIframeToContent may run more than once for the SAME iframe with
// DIFFERENT documents — Firefox-family browsers (Zen) can fire an extra load
// for the initial about:blank document before the srcdoc one. The
// ResizeObserver attach must be per-DOCUMENT, not a one-shot per-iframe
// boolean: latched on the blank document, the observer watches a dead body
// forever and the real email body is never observed (the "stuck partial"
// symptom persisting after the ceph fixes).
test('w5ba: a second call with a new document re-attaches the ResizeObserver to the new body', () => {
    const timers = fakeTimers();
    const roInstances = [];
    const sizeIframeToContent = loadSizeIframeToContent({
        ...timers,
        ResizeObserver: mockResizeObserver(roInstances),
    });

    const mkDoc = (h) => {
        const el = {
            get scrollHeight() { return h.value; },
            getBoundingClientRect: () => ({ height: h.value }),
        };
        return {
            body: el,
            documentElement: el,
            fonts: { ready: new Promise(() => {}) },
            querySelectorAll: () => [],
        };
    };

    const iframe = { style: { height: '0px' } };
    const hA = { value: 50 };
    iframe.contentDocument = mkDoc(hA); // the about:blank-era document
    sizeIframeToContent(iframe);
    timers.flush();
    assert.equal(curH(iframe), 50);

    // The real srcdoc document replaces the blank one; the browser fires a
    // second load and sizeIframeToContent runs again for the same iframe.
    const hB = { value: 120 };
    iframe.contentDocument = mkDoc(hB);
    sizeIframeToContent(iframe);
    timers.flush();
    assert.equal(curH(iframe), 120);

    // Late content grows the REAL document's body. Only an observer attached
    // to the new body can see this — the old `_sized` boolean left the RO on
    // the blank body and the iframe stayed clipped at 120.
    hB.value = 900;
    for (const ro of roInstances) ro.tick();
    timers.flush();
    assert.equal(
        curH(iframe),
        900,
        'the ResizeObserver must follow the current document — a one-shot per-iframe attach strands it on the about:blank body (w5ba)',
    );
});

// w5ba: every height write must be reported through iframe._onHeight so the
// caller can cache the settled height (the reopen fast path pre-sizes the
// iframe from it).
test('w5ba: height writes report through iframe._onHeight for the reopen height cache', () => {
    const timers = fakeTimers();
    const roInstances = [];
    const sizeIframeToContent = loadSizeIframeToContent({
        ...timers,
        ResizeObserver: mockResizeObserver(roInstances),
    });

    const img = makeImg();
    const { iframe, state } = makeMockIframe({
        partialHeight: 120,
        fullHeight: 1200,
        imgs: [img],
        bodyBorderBox: 'pinned',
    });
    const reported = [];
    iframe._onHeight = (h) => reported.push(h);

    sizeIframeToContent(iframe);
    timers.flush();
    state.imageLoaded = true;
    img.fire('load');
    timers.flush();

    assert.equal(curH(iframe), 1200);
    assert.equal(
        reported[reported.length - 1],
        1200,
        'the final height write must be reported via _onHeight so the reopen cache tracks the settled height (w5ba)',
    );
});

// ---------------------------------------------------------------------------
// renderHtmlBodyIframe: first-open hold-then-reveal + reopen fast path (w5ba)
// ---------------------------------------------------------------------------

function extractRenderHtmlBodyIframe(src) {
    const fnStart = src.indexOf('function renderHtmlBodyIframe');
    assert.notStrictEqual(fnStart, -1, 'function renderHtmlBodyIframe must exist in app.js');
    const close = src.indexOf('\n}', fnStart);
    assert.notStrictEqual(close, -1, 'renderHtmlBodyIframe must close with a column-0 brace');
    return src.slice(fnStart, close + 2);
}

// Load the real renderHtmlBodyIframe with faked collaborators. wrapEmailHtml/
// linkifyHtml become identity — the srcdoc string is not under test here.
function loadRenderHtmlBodyIframe({ document, setTimeout, clearTimeout, requestAnimationFrame, sizeIframeToContent }) {
    const code = extractRenderHtmlBodyIframe(APP_JS);
    // eslint-disable-next-line no-new-func
    return new Function(
        'document', 'setTimeout', 'clearTimeout', 'requestAnimationFrame', 'sizeIframeToContent',
        'wrapEmailHtml', 'linkifyHtml', 'EMAIL_IFRAME_REVEAL_CAP_MS',
        code + '\nreturn renderHtmlBodyIframe;',
    )(document, setTimeout, clearTimeout, requestAnimationFrame, sizeIframeToContent, (h) => h, (h) => h, 300);
}

function makeIframeEl() {
    const classes = new Set();
    const listeners = {};
    return {
        style: {},
        classList: {
            add: (c) => classes.add(c),
            remove: (c) => classes.delete(c),
            contains: (c) => classes.has(c),
        },
        setAttribute() {},
        addEventListener(type, fn) { (listeners[type] ||= []).push(fn); },
        fire(type) { for (const fn of listeners[type] || []) fn(); },
        contentDocument: null,
    };
}

function makeContainer() {
    const children = new Set();
    return {
        scrollTop: -1,
        querySelector: () => [...children][0] || null,
        replaceChildren() { children.clear(); },
        appendChild(el) { children.add(el); },
        contains: (el) => children.has(el),
    };
}

const realDoc = (fontsReady) => ({
    querySelector: (sel) => (sel === 'base' ? {} : null), // wrapEmailHtml injects <base>
    fonts: { ready: fontsReady },
});

test('w5ba: first open holds the iframe hidden (.settling) and reveals after fonts settle', async () => {
    const timers = fakeTimers();
    const sized = [];
    const iframeEl = makeIframeEl();
    const container = makeContainer();
    const render = loadRenderHtmlBodyIframe({
        document: { createElement: () => iframeEl },
        ...timers,
        sizeIframeToContent: (f) => sized.push(f),
    });

    render(container, '<p>hi</p>', { scrollTop: 42 });
    assert.ok(iframeEl.classList.contains('settling'), 'first open must start hidden');

    iframeEl.contentDocument = realDoc(Promise.resolve());
    iframeEl.fire('load');
    assert.equal(sized.length, 1, 'load must run the sizing machinery');
    assert.ok(iframeEl.classList.contains('settling'), 'still held until fonts settle + a frame');

    await new Promise((r) => setImmediate(r)); // fonts.ready.then(...)
    timers.flush(); // the rAF reveal
    assert.ok(!iframeEl.classList.contains('settling'), 'must reveal after fonts.ready + one frame');
    assert.equal(container.scrollTop, 42, 'scroll position restores at reveal');
});

test('w5ba: the reveal cap is armed at creation — the iframe can never be stranded invisible', () => {
    const timers = fakeTimers();
    const iframeEl = makeIframeEl();
    const container = makeContainer();
    const render = loadRenderHtmlBodyIframe({
        document: { createElement: () => iframeEl },
        ...timers,
        sizeIframeToContent: () => {},
    });

    render(container, '<p>hi</p>', {});
    assert.ok(iframeEl.classList.contains('settling'));
    // No load event ever fires (pathological). The creation-time cap alone
    // must still reveal the iframe.
    timers.flush();
    assert.ok(
        !iframeEl.classList.contains('settling'),
        'the time cap must reveal even if load never fires (w5ba)',
    );
});

test('w5ba: a load for a document without the wrapper <base> (Firefox about:blank) is skipped', () => {
    const timers = fakeTimers();
    const sized = [];
    const iframeEl = makeIframeEl();
    const container = makeContainer();
    const render = loadRenderHtmlBodyIframe({
        document: { createElement: () => iframeEl },
        ...timers,
        sizeIframeToContent: (f) => sized.push(f),
    });

    render(container, '<p>hi</p>', {});
    // Firefox fires an early load for the initial about:blank document — no
    // <base>, so it must be ignored (sizing it latches state on a dead doc).
    iframeEl.contentDocument = { querySelector: () => null, fonts: { ready: new Promise(() => {}) } };
    iframeEl.fire('load');
    assert.equal(sized.length, 0, 'the about:blank load must not run the sizing machinery');

    // The real srcdoc load follows and must size normally.
    iframeEl.contentDocument = realDoc(new Promise(() => {}));
    iframeEl.fire('load');
    assert.equal(sized.length, 1, 'the srcdoc load must run the sizing machinery');
});

// roborev 457 (Medium): a first-open iframe's reveal (cap timer or late
// fonts.ready continuation) must NOT fire against the shared container after
// a later render replaced the iframe — it would yank the NEW email's scroll
// position to the OLD email's saved offset.
test('roborev 457: a stale reveal must not clobber the next email\'s scroll position', async () => {
    const timers = fakeTimers();
    const els = [makeIframeEl(), makeIframeEl()];
    let n = 0;
    const container = makeContainer();
    const render = loadRenderHtmlBodyIframe({
        document: { createElement: () => els[n++] },
        ...timers,
        sizeIframeToContent: () => {},
    });

    // Email A: first open, held, saved scrollTop 42. Its fonts.ready resolves
    // LATE (after navigation) — arm the continuation now.
    render(container, '<p>A</p>', { scrollTop: 42 });
    els[0].contentDocument = realDoc(Promise.resolve());
    els[0].fire('load');

    // User navigates to email B before A's hold window closes. B is a
    // known-height reopen whose position restores to 7 on load.
    render(container, '<p>B</p>', { scrollTop: 7, knownHeight: 900 });
    els[1].contentDocument = realDoc(new Promise(() => {}));
    els[1].fire('load');
    assert.equal(container.scrollTop, 7);

    // A's stale cues fire now: the fonts.ready continuation and the cap timer.
    await new Promise((r) => setImmediate(r));
    timers.flush();
    assert.equal(
        container.scrollTop,
        7,
        'a replaced iframe\'s reveal must not write the container scrollTop (roborev 457)',
    );
});

test('w5ba: reopen with a known height pre-sizes the iframe and paints immediately (no hold)', () => {
    const timers = fakeTimers();
    const iframeEl = makeIframeEl();
    const container = makeContainer();
    const render = loadRenderHtmlBodyIframe({
        document: { createElement: () => iframeEl },
        ...timers,
        sizeIframeToContent: () => {},
    });

    render(container, '<p>hi</p>', { scrollTop: 7, knownHeight: 555 });
    assert.equal(iframeEl.style.height, '555px', 'reopen must pre-size the iframe before the srcdoc parses');
    assert.ok(!iframeEl.classList.contains('settling'), 'reopen must not hold — instant paint');

    iframeEl.contentDocument = realDoc(new Promise(() => {}));
    iframeEl.fire('load');
    assert.equal(container.scrollTop, 7, 'scroll position restores on load without waiting for fonts');
});

// Harness sanity + non-regression: when the body border-box is NOT pinned
// (body{height:auto}), the existing ResizeObserver path already grows the
// iframe — WITHOUT needing the image-load listener. This confirms the mock
// models the non-pinned case correctly and that the fix doesn't regress it.
test('control: RO grows the iframe when body border-box tracks content (non-pinned, no image listener)', () => {
    const timers = fakeTimers();
    const roInstances = [];
    const sizeIframeToContent = loadSizeIframeToContent({
        ...timers,
        ResizeObserver: mockResizeObserver(roInstances),
    });

    const img = makeImg();
    const { iframe, state } = makeMockIframe({
        partialHeight: 120,
        fullHeight: 1200,
        imgs: [img],
        bodyBorderBox: 'content',
    });

    sizeIframeToContent(iframe);
    timers.flush();
    assert.equal(curH(iframe), 120);

    state.imageLoaded = true;
    // Deliberately do NOT fire the image load listener — the RO alone must
    // catch the border-box growth.
    for (const ro of roInstances) ro.tick();
    timers.flush();

    assert.equal(
        curH(iframe),
        1200,
        'RO must grow the iframe when body border-box tracks content (non-pinned control)',
    );
});

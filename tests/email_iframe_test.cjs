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

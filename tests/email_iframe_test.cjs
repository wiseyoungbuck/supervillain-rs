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
function loadSizeIframeToContent({ requestAnimationFrame, setTimeout, clearTimeout, setInterval, clearInterval, ResizeObserver }) {
    const code = extractSizeIframeToContent(APP_JS);
    // eslint-disable-next-line no-new-func
    return new Function(
        'ResizeObserver',
        'requestAnimationFrame',
        'setTimeout',
        'clearTimeout',
        'setInterval',
        'clearInterval',
        code + '\nreturn sizeIframeToContent;',
    )(ResizeObserver, requestAnimationFrame, setTimeout, clearTimeout, setInterval, clearInterval);
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
    const iframe = { style: { height: '0px' }, isConnected: true };
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

// Ids are 1-based indices into a never-shrinking queue so clearTimeout can
// REALLY drop the queued callback — the roborev-459 regression test asserts
// the cancellation layer behaviorally, not just via the contains() backstop.
// Intervals live in their own id space (offset 1e6) so timeout and interval
// ids can't collide; tickIntervals() runs every live interval callback once.
function fakeTimers() {
    const queue = [];
    const intervals = [];
    return {
        requestAnimationFrame: (fn) => { queue.push(fn); return queue.length; },
        setTimeout: (fn) => { queue.push(fn); return queue.length; },
        clearTimeout: (id) => { if (id) queue[id - 1] = null; },
        setInterval: (fn) => { intervals.push(fn); return 1_000_000 + intervals.length; },
        clearInterval: (id) => { if (id >= 1_000_001) intervals[id - 1_000_001] = null; },
        tickIntervals: () => { for (const fn of [...intervals]) if (fn) fn(); },
        liveIntervals: () => intervals.filter(Boolean).length,
        pending: () => queue.filter(Boolean).length,
        flush: () => {
            let i = 0;
            while (i < queue.length) {
                const fn = queue[i];
                queue[i] = null;
                i++;
                if (fn) fn();
            }
        },
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

    const iframe = { style: { height: '0px' }, isConnected: true };
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

// w5ba round 2 (on-device repro): some emails still stick at the top inch —
// the cross-document ResizeObserver may never deliver in Firefox-family
// browsers, and table/CSS-driven late layout has no image events to hook.
// The safety-net poll must rescue ANY late content growth, through the
// grow path, gated by the ratchet epsilon.
test('w5ba: the safety-net poll rescues a stuck-partial iframe when every other cue is dead', () => {
    const timers = fakeTimers();
    const roInstances = [];
    const sizeIframeToContent = loadSizeIframeToContent({
        ...timers,
        ResizeObserver: mockResizeObserver(roInstances),
    });

    // No images (no load/error cues), pinned body (RO never fires).
    const { iframe, state } = makeMockIframe({
        partialHeight: 120,
        fullHeight: 1200,
        imgs: [],
        bodyBorderBox: 'pinned',
    });

    sizeIframeToContent(iframe);
    timers.flush();
    assert.equal(curH(iframe), 120);

    // Late layout (a big table, CSS) grows the content. No cue fires: RO is
    // pinned-blind, there are no images, fonts never resolve.
    state.imageLoaded = true;
    for (const ro of roInstances) ro.tick();
    assert.equal(curH(iframe), 120, 'no other cue may rescue this scenario (harness sanity)');

    timers.tickIntervals(); // the poll
    timers.flush();
    assert.equal(
        curH(iframe),
        1200,
        'the safety-net poll must grow a stuck-partial iframe when RO/image/font cues are all dead (w5ba)',
    );
});

test('w5ba: the poll ignores sub-epsilon growth so viewport-relative sender CSS cannot self-feed', () => {
    const timers = fakeTimers();
    const sizeIframeToContent = loadSizeIframeToContent({
        ...timers,
        ResizeObserver: mockResizeObserver([]),
    });

    const { iframe, state } = makeMockIframe({
        partialHeight: 120,
        fullHeight: 150, // +30px — under EMAIL_IFRAME_RATCHET_EPSILON (64)
        imgs: [],
        bodyBorderBox: 'pinned',
    });

    sizeIframeToContent(iframe);
    timers.flush();
    state.imageLoaded = true;
    timers.tickIntervals();
    timers.flush();
    assert.equal(
        curH(iframe),
        120,
        'sub-epsilon growth must not trigger the poll — the min-height:100vh ratchet tracks our own writes by less than epsilon (w5ba)',
    );
});

test('w5ba: the poll self-clears once its iframe leaves the DOM', () => {
    const timers = fakeTimers();
    const sizeIframeToContent = loadSizeIframeToContent({
        ...timers,
        ResizeObserver: mockResizeObserver([]),
    });

    const { iframe, state } = makeMockIframe({
        partialHeight: 120,
        fullHeight: 1200,
        imgs: [],
        bodyBorderBox: 'pinned',
    });

    sizeIframeToContent(iframe);
    timers.flush();
    assert.equal(timers.liveIntervals(), 1, 'sizing arms exactly one poll');

    // Plain-text render / view switch detaches the iframe without coming
    // through renderHtmlBodyIframe's teardown.
    iframe.isConnected = false;
    state.imageLoaded = true;
    timers.tickIntervals();
    timers.flush();
    assert.equal(curH(iframe), 120, 'a detached iframe must not keep being sized');
    assert.equal(timers.liveIntervals(), 0, 'the poll must clearInterval itself on detach (w5ba)');
});

// A viewport-feedback ratchet mock: content is always `gap` px taller than
// whatever the iframe currently is — every poll tick sees h - cur >= gap,
// forever, because the content chases our own writes.
function makeFeedbackIframe(gap) {
    const iframe = { style: { height: '0px' }, isConnected: true };
    const el = {
        get scrollHeight() { return (parseFloat(iframe.style.height) || 100) + gap; },
        getBoundingClientRect() { return { height: this.scrollHeight }; },
    };
    iframe.contentDocument = {
        body: el,
        documentElement: el,
        fonts: { ready: new Promise(() => {}) },
        querySelectorAll: () => [],
    };
    return iframe;
}

// roborev 461/463: sender CSS whose viewport-relative feedback EXCEEDS the
// epsilon (min-height:110vh, big body margins) satisfies the poll's gate on
// every tick. After the 3-grow allowance, the poll must degrade to a LINEAR
// clamped crawl: at most one write per ~3 ticks (the stability-probe
// cadence), each write bounded by the probe clamp (4 * epsilon = 256px) —
// never a full-h write, which made the crawl exponential (roborev 463).
test('roborev 461/463: a supra-epsilon ratchet degrades to a sparse, clamped linear crawl', () => {
    const timers = fakeTimers();
    const sizeIframeToContent = loadSizeIframeToContent({
        ...timers,
        ResizeObserver: mockResizeObserver([]),
    });

    const iframe = makeFeedbackIframe(500); // chase gap far above the clamp
    const writes = [];
    iframe._onHeight = (h) => writes.push(h);

    sizeIframeToContent(iframe);
    timers.flush();
    for (let i = 0; i < 3; i++) timers.tickIntervals(); // the streak allowance

    const allowanceEnd = writes.length;
    let prev = curH(iframe);
    const postDeltas = [];
    for (let i = 0; i < 15; i++) {
        const before = writes.length;
        timers.tickIntervals();
        if (writes.length > before) postDeltas.push(curH(iframe) - prev);
        prev = curH(iframe);
    }
    // Sparse: the probe cadence is one write per ~3 ticks.
    assert.ok(
        postDeltas.length <= 6,
        `post-allowance ratchet writes must be sparse (probe cadence ~1 per 3 ticks); got ${postDeltas.length} in 15 ticks`,
    );
    // Linear: every post-allowance write is clamped — a full-h write here
    // means the crawl went exponential again (roborev 463).
    for (const d of postDeltas) {
        assert.ok(
            d <= 4 * 64,
            `post-allowance ratchet writes must be clamped to 4*epsilon; got a ${d}px write`,
        );
    }
    assert.ok(allowanceEnd >= 1, 'harness sanity: the allowance phase grew');
});

// roborev 462: the streak reset must keep the poll alive for a real stuck
// email that needs MANY rescues — supra-epsilon jumps separated by catch-up
// ticks must all apply, no matter how many.
test('roborev 462: jump/settle cycles reset the streak — every legitimate rescue applies', () => {
    const timers = fakeTimers();
    const sizeIframeToContent = loadSizeIframeToContent({
        ...timers,
        ResizeObserver: mockResizeObserver([]),
    });

    const content = { value: 120 };
    const iframe = { style: { height: '0px' }, isConnected: true };
    const el = {
        get scrollHeight() { return content.value; },
        getBoundingClientRect() { return { height: content.value }; },
    };
    iframe.contentDocument = {
        body: el,
        documentElement: el,
        fonts: { ready: new Promise(() => {}) },
        querySelectorAll: () => [],
    };

    sizeIframeToContent(iframe);
    timers.flush();
    assert.equal(curH(iframe), 120);

    for (let i = 0; i < 6; i++) {
        content.value += 500; // supra-epsilon late layout
        timers.tickIntervals(); // grow applies, cur catches up
        timers.tickIntervals(); // settled tick — must reset the streak
    }
    assert.equal(
        curH(iframe),
        120 + 6 * 500,
        'every jump separated by a catch-up tick must apply — the streak reset keeps the poll alive (roborev 462)',
    );
});

// roborev 462: suppression must not be a permanent lockout. After the streak
// trips, a locked-out email whose content is STABLE (unlike a ratchet, whose
// content chases every write) gets a recovery probe grow.
test('roborev 462: a locked-out poll recovers via the stability probe once content stops moving', () => {
    const timers = fakeTimers();
    const sizeIframeToContent = loadSizeIframeToContent({
        ...timers,
        ResizeObserver: mockResizeObserver([]),
    });

    const content = { value: 100 };
    const iframe = { style: { height: '0px' }, isConnected: true };
    const el = {
        get scrollHeight() { return content.value; },
        getBoundingClientRect() { return { height: content.value }; },
    };
    iframe.contentDocument = {
        body: el,
        documentElement: el,
        fonts: { ready: new Promise(() => {}) },
        querySelectorAll: () => [],
    };

    sizeIframeToContent(iframe);
    timers.flush();

    // Four consecutive supra-epsilon growth ticks: 3 apply, the 4th trips
    // the streak and is suppressed.
    for (const v of [600, 1200, 1800, 2400]) {
        content.value = v;
        timers.tickIntervals();
    }
    assert.equal(curH(iframe), 1800, 'the 4th consecutive grow must be suppressed');

    // Content now sits still at 2400 — a real email, not a ratchet. Two
    // stable suppressed ticks earn the CLAMPED probe (1800 + 4*64 = 2056,
    // roborev 463); content does not chase it, which proves this is not a
    // ratchet, so the next tick lifts the suppression and grows fully.
    timers.tickIntervals();
    timers.tickIntervals();
    assert.equal(curH(iframe), 2056, 'the probe must be clamped to 4*epsilon (roborev 463)');
    timers.tickIntervals();
    assert.equal(
        curH(iframe),
        2400,
        'content not chasing the probe proves a real email — suppression lifts and the full grow applies (roborev 462/463)',
    );
});

// roborev 463: a same-iframe document swap (the belt-and-braces re-attach)
// must not inherit the previous document's suppression — the new document's
// first supra-epsilon growth must apply immediately.
test('roborev 463: a document swap resets the poll streak — the new document starts unsuppressed', () => {
    const timers = fakeTimers();
    const sizeIframeToContent = loadSizeIframeToContent({
        ...timers,
        ResizeObserver: mockResizeObserver([]),
    });

    // Document A is a ratchet: drive the streak into lockout.
    const iframe = makeFeedbackIframe(500);
    sizeIframeToContent(iframe);
    timers.flush();
    for (let i = 0; i < 6; i++) timers.tickIntervals();

    // The document is swapped (Firefox about:blank → srcdoc path) and the
    // sizing machinery re-runs for the same iframe element.
    const content = { value: curH(iframe) + 900 };
    const el = {
        get scrollHeight() { return content.value; },
        getBoundingClientRect() { return { height: content.value }; },
    };
    iframe.contentDocument = {
        body: el,
        documentElement: el,
        fonts: { ready: new Promise(() => {}) },
        querySelectorAll: () => [],
    };
    sizeIframeToContent(iframe);
    timers.flush();

    const before = curH(iframe);
    content.value = before + 800; // first supra-epsilon growth on doc B
    timers.tickIntervals();
    assert.equal(
        curH(iframe),
        before + 800,
        'the first supra-epsilon tick on a swapped-in document must grow — the streak must not carry across documents (roborev 463)',
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
function loadRenderHtmlBodyIframe({ document, setTimeout, clearTimeout, clearInterval, requestAnimationFrame, sizeIframeToContent }) {
    const code = extractRenderHtmlBodyIframe(APP_JS);
    // eslint-disable-next-line no-new-func
    return new Function(
        'document', 'setTimeout', 'clearTimeout', 'clearInterval', 'requestAnimationFrame', 'sizeIframeToContent',
        'wrapEmailHtml', 'linkifyHtml', 'EMAIL_IFRAME_REVEAL_CAP_MS',
        code + '\nreturn renderHtmlBodyIframe;',
    )(document, setTimeout, clearTimeout, clearInterval, requestAnimationFrame, sizeIframeToContent, (h) => h, (h) => h, 300);
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
    assert.equal(timers.pending(), 1, 'A arms exactly its reveal cap timer');

    // User navigates to email B before A's hold window closes. B is a
    // known-height reopen whose position restores to 7 on load.
    render(container, '<p>B</p>', { scrollTop: 7, knownHeight: 900 });
    // First cancellation layer: teardown must clearTimeout A's cap (B holds
    // nothing, so no pending timer may remain).
    assert.equal(timers.pending(), 0, 'teardown must cancel A\'s reveal cap timer (roborev 457/459)');
    els[1].contentDocument = realDoc(new Promise(() => {}));
    els[1].fire('load');
    assert.equal(container.scrollTop, 7);

    // A's remaining stale cue fires now: the fonts.ready continuation (its
    // cap timer is already cancelled). The contains() backstop must block it.
    await new Promise((r) => setImmediate(r));
    timers.flush();
    assert.equal(
        container.scrollTop,
        7,
        'a replaced iframe\'s reveal must not write the container scrollTop (roborev 457)',
    );
});

// roborev 461: the teardown path must actively stop the prior iframe's poll —
// the isConnected self-clear is only the backstop for renders that bypass
// renderHtmlBodyIframe.
test('roborev 461: rendering over an old iframe clears its safety-net poll', () => {
    const timers = fakeTimers();
    const iframeEl = makeIframeEl();
    const container = makeContainer();
    const render = loadRenderHtmlBodyIframe({
        document: { createElement: () => iframeEl },
        ...timers,
        sizeIframeToContent: () => {},
    });

    // A previously-rendered iframe with a live poll sits in the container.
    const old = makeIframeEl();
    old.className = 'email-iframe';
    old._pollTimer = timers.setInterval(() => {});
    container.appendChild(old);
    assert.equal(timers.liveIntervals(), 1);

    render(container, '<p>next</p>', {});
    assert.equal(
        timers.liveIntervals(),
        0,
        'renderHtmlBodyIframe must clearInterval the prior iframe\'s poll on teardown (roborev 461)',
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

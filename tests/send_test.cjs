// Characterization + behavior tests for the desktop send path (kata vj6k /
// acag). The characterization block pins doSendEmail's CURRENT observable
// contract — request shape, autosave-gate scoping, session guards, failure
// surfacing — so the deferred-send work provably cannot regress it. These
// stay green throughout the red/green cycle.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const APP_JS = fs.readFileSync(path.join(__dirname, '..', 'static', 'app.js'), 'utf8');

function extractFunction(declaration) {
    const start = APP_JS.indexOf(declaration);
    assert.notStrictEqual(start, -1, `${declaration} must exist`);
    const close = APP_JS.indexOf('\n}', start);
    assert.notStrictEqual(close, -1, `${declaration} must close`);
    return APP_JS.slice(start, close + 2);
}

// Harness around the real doSendEmail body. Every module-level name the
// function reads is injected as a parameter, so the tests drive the exact
// shipped code with observable stubs.
function sendHarness({
    apiImpl = null,
    to = 'to@x.com',
    composeSession = 1,
    sendingSession = 1,
    trackedDraftSession = 1,
    trackedDraftId = 'd-1',
    draftId = 'd-1',
    pendingAttachments = [],
    replyContext = null,
} = {}) {
    const state = {
        pendingAttachments,
        replyContext,
        composeSession,
        draftId,
        timezone: null,
    };
    const els = {
        composeTo: { value: to },
        composeCc: { value: '' },
        composeFrom: { value: 'me@example.com' },
        composeSubject: { value: 'Subj' },
        composeBody: { value: 'Body' },
        composeInviteEnabled: { checked: false },
    };
    const out = {
        state,
        els,
        apiCalls: [],
        statuses: [],
        deleted: [],
        views: [],
        cleared: 0,
        autosaveCancels: 0,
    };
    const api = async (method, path, body) => {
        out.apiCalls.push({ method, path, body });
        if (apiImpl) return apiImpl(method, path, body);
        return { success: true };
    };
    const escapeHtml = (s) => String(s)
        .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
    const code = [
        extractFunction('async function doSendEmail('),
        'return doSendEmail;',
    ].join('\n');
    // eslint-disable-next-line no-new-func
    const doSendEmail = new Function(
        'state', 'els', 'api', 'showStatus', 'escapeHtml', 'cancelAutosave',
        'sendingSession', 'saveInFlight', 'trackedDraftSession',
        'trackedDraftId', 'deleteDraftById', 'clearCompose', 'showView',
        // Deferral collaborators (kata vj6k/acag), neutralized so the
        // characterized contract is exactly the pre-deferral behavior:
        // window disabled, no picker hand-off.
        'undoSendDelaySecs', 'showSendUndoToast', 'sendLaterAt',
        code,
    )(
        state, els, api,
        (msg, kind) => out.statuses.push([msg, kind]),
        escapeHtml,
        () => { out.autosaveCancels++; },
        sendingSession, null, trackedDraftSession, trackedDraftId,
        (id) => out.deleted.push(id),
        () => { out.cleared++; },
        (view) => out.views.push(view),
        () => 0, () => {}, null,
    );
    out.doSendEmail = doSendEmail;
    return out;
}

// ---------------------------------------------------------------------------
// Characterization: the shipped contract (green before and after vj6k/acag)
// ---------------------------------------------------------------------------

test('send: POSTs the compose snapshot to /emails/send and finishes the compose', async () => {
    const h = sendHarness();
    await h.doSendEmail();
    assert.equal(h.apiCalls.length, 1);
    const { method, path, body } = h.apiCalls[0];
    assert.equal(method, 'POST');
    assert.equal(path, '/emails/send');
    assert.deepEqual(body.to, ['to@x.com']);
    assert.deepEqual(body.cc, []);
    assert.equal(body.subject, 'Subj');
    assert.equal(body.body, 'Body');
    assert.equal(body.html_body, undefined);
    assert.equal(body.in_reply_to, null);
    assert.equal(body.from_address, 'me@example.com');
    assert.equal(body.attachments, undefined);
    assert.deepEqual(h.statuses, [['Sent!', 'success']]);
    assert.deepEqual(h.deleted, ['d-1']);
    assert.equal(h.cleared, 1);
    assert.deepEqual(h.views, ['list']);
});

test('send: reply context quotes the original and threads in_reply_to', async () => {
    const h = sendHarness({
        replyContext: { quotedText: 'orig line', quotedHtml: '<p>orig</p>', inReplyTo: 'm-9' },
    });
    await h.doSendEmail();
    const { body } = h.apiCalls[0];
    assert.ok(body.body.endsWith('> orig line'), 'plain body must quote the original');
    assert.ok(body.html_body.includes('<blockquote'), 'html body must blockquote the original');
    assert.equal(body.in_reply_to, 'm-9');
});

test('send: ready attachments ride along; none means the field is omitted', async () => {
    const h = sendHarness({
        pendingAttachments: [
            { status: 'ready', blob_id: 'b-1', name: 'a.txt', mime_type: 'text/plain', size: 3 },
            { status: 'failed', blob_id: 'b-2', name: 'x.txt', mime_type: 'text/plain', size: 9 },
        ],
    });
    await h.doSendEmail();
    assert.deepEqual(h.apiCalls[0].body.attachments, [
        { blob_id: 'b-1', name: 'a.txt', mime_type: 'text/plain', size: 3 },
    ]);
});

test('send: no recipients refuses without touching the network', async () => {
    const h = sendHarness({ to: '  ' });
    await h.doSendEmail();
    assert.equal(h.apiCalls.length, 0);
    assert.deepEqual(h.statuses, [['No recipients', 'error']]);
    assert.equal(h.cleared, 0);
});

test('send: an in-flight upload blocks the send', async () => {
    const h = sendHarness({
        pendingAttachments: [{ status: 'uploading', blob_id: null, name: 'big.bin' }],
    });
    await h.doSendEmail();
    assert.equal(h.apiCalls.length, 0);
    assert.deepEqual(h.statuses, [['Wait for uploads to finish', 'error']]);
});

test('send: failure surfaces, keeps the draft, and never clears the compose', async () => {
    const h = sendHarness({ apiImpl: () => Promise.reject(new Error('boom')) });
    await h.doSendEmail();
    assert.deepEqual(h.statuses, [['Send failed: boom', 'error']]);
    assert.deepEqual(h.deleted, []);
    assert.equal(h.cleared, 0);
    assert.deepEqual(h.views, []);
});

test('send: completion only clears a compose the send still owns', async () => {
    // The user Escaped and started a new compose (session bumped) while the
    // send was in flight: the draft delete still fires (tracked pair says no
    // recapture), but clear/navigate must not yank the new compose.
    const h = sendHarness({ composeSession: 2, sendingSession: 1 });
    await h.doSendEmail();
    assert.deepEqual(h.deleted, ['d-1']);
    assert.equal(h.cleared, 0);
    assert.deepEqual(h.views, []);
});

test('send: a reopened draft that recaptured the id is not deleted', async () => {
    // roborev 316/317: leave-mid-send then reopen-from-Drafts adopts the SAME
    // id under a newer tracking session — deleting it would pull the draft
    // out from under the live editor.
    const h = sendHarness({ trackedDraftSession: 2, trackedDraftId: 'd-1' });
    await h.doSendEmail();
    assert.deepEqual(h.deleted, []);
});

test('send: the post-settle autosave cancel is scoped to the sending session', async () => {
    // The pinned gate line (roborev 319): cancel fires twice (top + post-
    // settle) only while the compose being sent is still current; a compose
    // the user reopened mid-send keeps its timer.
    const own = sendHarness({ composeSession: 1, sendingSession: 1 });
    await own.doSendEmail();
    assert.equal(own.autosaveCancels, 2);
    const other = sendHarness({ composeSession: 2, sendingSession: 1 });
    await other.doSendEmail();
    assert.equal(other.autosaveCancels, 1);
});

// ---------------------------------------------------------------------------
// Deferred send (kata vj6k Undo Send + kata acag Send Later)
// ---------------------------------------------------------------------------

const API_JS = fs.readFileSync(path.join(__dirname, '..', 'static', 'api.js'), 'utf8');

function classListStub(initialHidden = true) {
    const classes = new Set(initialHidden ? ['hidden'] : []);
    return {
        add: (c) => classes.add(c),
        remove: (c) => classes.delete(c),
        contains: (c) => classes.has(c),
    };
}

// Deferred-send harness: the characterization harness plus the deferral
// collaborators (delay setting, undo toast, the picker's module slot).
function deferHarness({ apiImpl, delaySecs = 0, sendLaterAt = null, ...rest } = {}) {
    const base = { apiImpl, ...rest };
    const state = {
        pendingAttachments: base.pendingAttachments || [],
        replyContext: base.replyContext || null,
        composeSession: 1,
        draftId: 'd-1',
        timezone: null,
    };
    const els = {
        composeTo: { value: 'to@x.com' },
        composeCc: { value: '' },
        composeFrom: { value: 'me@example.com' },
        composeSubject: { value: 'Subj' },
        composeBody: { value: 'Body' },
        composeInviteEnabled: { checked: false },
    };
    const out = {
        state, els, apiCalls: [], statuses: [], deleted: [], views: [],
        cleared: 0, toasts: [],
    };
    const api = async (method, path, body) => {
        out.apiCalls.push({ method, path, body });
        if (apiImpl) return apiImpl(method, path, body);
        return { success: true };
    };
    const code = [
        extractFunction('async function doSendEmail('),
        'return doSendEmail;',
    ].join('\n');
    // eslint-disable-next-line no-new-func
    out.doSendEmail = new Function(
        'state', 'els', 'api', 'showStatus', 'escapeHtml', 'cancelAutosave',
        'sendingSession', 'saveInFlight', 'trackedDraftSession',
        'trackedDraftId', 'deleteDraftById', 'clearCompose', 'showView',
        'undoSendDelaySecs', 'showSendUndoToast', 'sendLaterAt',
        code,
    )(
        state, els, api,
        (msg, kind) => out.statuses.push([msg, kind]),
        (s) => String(s), () => {},
        1, null, 1, 'd-1',
        (id) => out.deleted.push(id),
        () => { out.cleared++; },
        (view) => out.views.push(view),
        () => delaySecs,
        (id, deadline) => out.toasts.push({ id, deadline }),
        sendLaterAt,
    );
    return out;
}

test('vj6k: the undo window defers the send and raises the countdown toast', async () => {
    const before = Date.now();
    const h = deferHarness({
        delaySecs: 10,
        apiImpl: () => ({ success: true, scheduled: true, id: 'q-1' }),
    });
    await h.doSendEmail();
    const sendAt = h.apiCalls[0].body.send_at;
    assert.ok(sendAt, 'the POST must carry send_at when the undo window is on');
    const delta = new Date(sendAt).getTime() - before;
    assert.ok(delta >= 9_000 && delta <= 12_000,
        `send_at must be ~10s out, got ${delta}ms`);
    assert.equal(h.toasts.length, 1, 'the undo toast must be shown');
    assert.equal(h.toasts[0].id, 'q-1');
    assert.ok(!h.statuses.some(([msg]) => msg === 'Sent!'),
        'a deferred send must not claim Sent!');
    // The compose finishes exactly like a sent mail: the queued record now
    // owns the content; Undo restores it from there.
    assert.deepEqual(h.deleted, ['d-1']);
    assert.equal(h.cleared, 1);
    assert.deepEqual(h.views, ['list']);
});

test('vj6k: a zero undo window sends immediately with no toast', async () => {
    const h = deferHarness({ delaySecs: 0 });
    await h.doSendEmail();
    assert.equal(h.apiCalls[0].body.send_at, undefined);
    assert.deepEqual(h.statuses, [['Sent!', 'success']]);
    assert.equal(h.toasts.length, 0);
});

test('acag: an explicit Send Later time wins over the undo window', async () => {
    const later = new Date(Date.now() + 3 * 3600_000);
    const h = deferHarness({
        delaySecs: 10,
        sendLaterAt: later,
        apiImpl: () => ({ success: true, scheduled: true, id: 'q-2' }),
    });
    await h.doSendEmail();
    assert.equal(h.apiCalls[0].body.send_at, later.toISOString());
    assert.equal(h.toasts.length, 0, 'an explicit schedule is not an undo window');
    assert.ok(h.statuses.some(([msg, kind]) =>
        kind === 'success' && msg.startsWith('Scheduled for')),
        'the user must see when the mail will go out');
    assert.deepEqual(h.deleted, ['d-1']);
});

test('vj6k: undoSendDelaySecs reads localStorage with default 10, clamped 0..60', () => {
    const code = [
        extractFunction('function undoSendDelaySecs('),
        'return undoSendDelaySecs;',
    ].join('\n');
    const at = (stored) => {
        // eslint-disable-next-line no-new-func
        const fn = new Function('localStorage', code)({ getItem: () => stored });
        return fn();
    };
    assert.equal(at(null), 10, 'unset defaults to 10s');
    assert.equal(at(''), 10);
    assert.equal(at('garbage'), 10);
    assert.equal(at('25'), 25);
    assert.equal(at('0'), 0, '0 disables the window');
    assert.equal(at('-5'), 0);
    assert.equal(at('999'), 60, 'clamped to 60s');
});

test('vj6k: cancelScheduledSend restores the compose draft from the record', async () => {
    const record = {
        id: 'q-1',
        from_addr: 'me@example.com',
        submission: {
            to: ['a@x.com', 'b@x.com'],
            cc: ['c@x.com'],
            subject: 'Deferred hello',
            text_body: 'see you\n\n> quoted',
            in_reply_to: 'm-9',
            attachments: [{ blob_id: 'b-1', name: 'a.txt', mime_type: 'text/plain', size: 3 }],
        },
    };
    const state = { pendingAttachments: [], replyContext: null };
    const els = {
        composeTo: { value: '' }, composeCc: { value: '' },
        composeSubject: { value: '' }, composeBody: { value: '' },
        composeFrom: { value: '' },
    };
    const out = { apiCalls: [], statuses: [], views: [], cleared: 0, rendered: 0 };
    const api = async (method, path) => {
        out.apiCalls.push({ method, path });
        return record;
    };
    const code = [
        extractFunction('async function cancelScheduledSend('),
        extractFunction('function restoreComposeFromScheduledSend('),
        'return cancelScheduledSend;',
    ].join('\n');
    // eslint-disable-next-line no-new-func
    const cancel = new Function(
        'state', 'els', 'api', 'showStatus', 'clearCompose', 'showView',
        'renderComposeAttachments',
        code,
    )(
        state, els, api,
        (msg, kind) => out.statuses.push([msg, kind]),
        () => { out.cleared++; },
        (view) => out.views.push(view),
        () => { out.rendered++; },
    );
    await cancel('q-1');
    assert.deepEqual(out.apiCalls, [{ method: 'DELETE', path: '/scheduled-sends/q-1' }]);
    assert.equal(out.cleared, 1, 'restore starts from a clean compose session');
    assert.equal(els.composeTo.value, 'a@x.com, b@x.com');
    assert.equal(els.composeCc.value, 'c@x.com');
    assert.equal(els.composeSubject.value, 'Deferred hello');
    assert.equal(els.composeBody.value, 'see you\n\n> quoted');
    assert.equal(els.composeFrom.value, 'me@example.com');
    assert.equal(state.replyContext.inReplyTo, 'm-9', 'threading survives the round trip');
    assert.equal(state.pendingAttachments.length, 1);
    assert.equal(state.pendingAttachments[0].status, 'ready');
    assert.equal(out.rendered, 1);
    assert.deepEqual(out.views, ['compose']);
    assert.ok(out.statuses.some(([, kind]) => kind === 'success'));
});

test('vj6k: cancelScheduledSend after dispatch surfaces "too late"', async () => {
    const err = new Error('no scheduled send');
    err.status = 404;
    const out = { statuses: [], views: [] };
    const code = [
        extractFunction('async function cancelScheduledSend('),
        extractFunction('function restoreComposeFromScheduledSend('),
        'return cancelScheduledSend;',
    ].join('\n');
    // eslint-disable-next-line no-new-func
    const cancel = new Function(
        'state', 'els', 'api', 'showStatus', 'clearCompose', 'showView',
        'renderComposeAttachments',
        code,
    )(
        {}, {}, async () => { throw err; },
        (msg, kind) => out.statuses.push([msg, kind]),
        () => {}, (view) => out.views.push(view), () => {},
    );
    await cancel('q-1');
    assert.deepEqual(out.statuses, [['Too late — already sent', 'error']]);
    assert.deepEqual(out.views, [], 'no restore after the mail has departed');
});

test('vj6k: the undo toast counts down and clears itself at zero', () => {
    let fakeNow = 0;
    let tick = null;
    const state = { sendUndo: null };
    const els = {
        sendUndoToast: { classList: classListStub(true) },
        sendUndoMessage: { textContent: '' },
    };
    const cleared = [];
    const code = [
        extractFunction('function showSendUndoToast('),
        extractFunction('function clearSendUndoToast('),
        'return showSendUndoToast;',
    ].join('\n');
    // eslint-disable-next-line no-new-func
    const show = new Function(
        'state', 'els', 'setInterval', 'clearInterval', 'Date',
        code,
    )(
        state, els,
        (fn) => { tick = fn; return 7; },
        (id) => cleared.push(id),
        { now: () => fakeNow },
    );
    show('q-1', new Date(10_000));
    assert.equal(els.sendUndoMessage.textContent, 'Sending in 10s');
    assert.ok(!els.sendUndoToast.classList.contains('hidden'), 'toast must show');
    assert.equal(state.sendUndo.id, 'q-1');
    fakeNow = 9_600;
    tick();
    assert.equal(els.sendUndoMessage.textContent, 'Sending in 1s');
    fakeNow = 10_500;
    tick();
    assert.ok(els.sendUndoToast.classList.contains('hidden'), 'toast hides at zero');
    assert.equal(state.sendUndo, null);
    assert.deepEqual(cleared, [7], 'the countdown interval must be stopped');
});

test('acag: confirmSendLaterAt rejects past times and schedules future ones', () => {
    const out = { statuses: [], closed: 0, sends: 0 };
    const code = [
        extractFunction('function confirmSendLaterAt('),
        'return { confirmSendLaterAt, probe: () => sendLaterAt };',
    ].join('\n');
    // eslint-disable-next-line no-new-func
    const { confirmSendLaterAt, probe } = new Function(
        'showStatus', 'closeSendLaterPicker', 'sendEmail', 'sendLaterAt',
        code,
    )(
        (msg, kind) => out.statuses.push([msg, kind]),
        () => { out.closed++; },
        () => { out.sends++; },
        null,
    );
    confirmSendLaterAt(new Date(Date.now() - 1000));
    assert.equal(out.sends, 0, 'a past time must not send');
    assert.deepEqual(out.statuses, [['Choose a future send time', 'error']]);
    const future = new Date(Date.now() + 3600_000);
    confirmSendLaterAt(future);
    assert.equal(out.closed, 1);
    assert.equal(out.sends, 1);
    assert.equal(probe(), future, 'the picked time must be handed to the send path');
});

test('acag: resolveSendLaterInput parses the picker inputs like Remind Me', () => {
    const fields = { 'send-later-natural': { value: '' }, 'send-later-datetime': { value: '' } };
    const code = [
        extractFunction('function resolveSendLaterInput('),
        extractFunction('function resolveTimeInput('),
        'return resolveSendLaterInput;',
    ].join('\n');
    // eslint-disable-next-line no-new-func
    const resolve = new Function(
        'document', 'localReminderDate',
        code,
    )(
        { getElementById: (id) => fields[id] },
        (days) => {
            const d = new Date();
            d.setDate(d.getDate() + days);
            d.setHours(8, 0, 0, 0);
            return d;
        },
    );
    fields['send-later-datetime'].value = '2030-01-02T15:00';
    assert.equal(resolve().getTime(), new Date('2030-01-02T15:00').getTime(),
        'an explicit datetime wins');
    fields['send-later-datetime'].value = '';
    fields['send-later-natural'].value = '3h';
    const delta = resolve().getTime() - Date.now();
    assert.ok(delta > 2.9 * 3600_000 && delta < 3.1 * 3600_000, '3h shorthand');
    fields['send-later-natural'].value = 'tomorrow 9am';
    assert.equal(resolve().getHours(), 9, 'tomorrow 9am sets the hour');
    fields['send-later-natural'].value = '';
    assert.equal(resolve(), null, 'empty input resolves to nothing');
});

test('vj6k/acag: scheduled-sends is an account-scoped api.js path', () => {
    const m = API_JS.match(/const ACCOUNT_SCOPED_API = (.*);/);
    assert.ok(m, 'ACCOUNT_SCOPED_API must exist in api.js');
    assert.ok(m[1].includes('scheduled-sends'),
        'GET /scheduled-sends must carry ?account= like the other scoped routes');
});

// ---------------------------------------------------------------------------
// Open tracking (kata e2h4) — minimal read-status UI
// ---------------------------------------------------------------------------

test('e2h4: readStatusRows renders open counts and escapes subjects', () => {
    const code = [
        extractFunction('function readStatusRows('),
        'return readStatusRows;',
    ].join('\n');
    const escapeHtml = (s) => String(s)
        .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
    // eslint-disable-next-line no-new-func
    const rows = new Function('escapeHtml', code)(escapeHtml);
    const html = rows([
        {
            subject: '<img src=x onerror=alert(1)>',
            recipients: ['a@x.com'],
            opens: ['2030-01-02T15:00:00Z', '2030-01-02T16:00:00Z'],
        },
        { subject: 'Quiet one', recipients: ['b@x.com'], opens: [] },
    ]);
    assert.ok(!html.includes('<img src=x'), 'subjects must be escaped');
    assert.ok(html.includes('&lt;img'), 'escaped subject still shown');
    assert.ok(/Opened 2×/.test(html), 'open count shown');
    assert.ok(html.includes('Not opened'), 'unopened sends say so');
});

test('e2h4: tracking is an account-scoped api.js path', () => {
    const m = API_JS.match(/const ACCOUNT_SCOPED_API = (.*);/);
    assert.ok(m, 'ACCOUNT_SCOPED_API must exist in api.js');
    assert.ok(m[1].includes('tracking'),
        'GET /tracking/status must carry ?account= like the other scoped routes');
});

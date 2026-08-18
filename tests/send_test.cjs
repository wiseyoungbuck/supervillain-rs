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

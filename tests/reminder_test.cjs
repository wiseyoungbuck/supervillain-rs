// Runtime contract tests for the real Remind Me client functions (kata dd0d).
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

function makeHarness(fetchImpl) {
    const state = {
        emails: [{ id: 'email-1', subject: 'Follow up' }],
        undoStack: [],
        selectedIndex: 0,
        currentMailbox: { role: 'inbox' },
        view: 'list',
    };
    const calls = [];
    const api = async (method, path, body) => {
        calls.push({ method, path, body });
        if (fetchImpl) return fetchImpl(method, path, body);
        return { success: true };
    };
    const pushUndo = (action, emailId, emailData, insertIndex, reminder) => {
        state.undoStack.push({ action, emailId, emailData, insertIndex, reminder });
    };
    const removeEmailFromList = (id) => {
        state.emails = state.emails.filter((email) => email.id !== id);
    };
    const refillSuppressedIds = new Set();
    const code = [
        extractFunction('async function remindEmail('),
        'return remindEmail;',
    ].join('\n');
    // eslint-disable-next-line no-new-func
    const remindEmail = new Function(
        'state', 'api', 'pushUndo', 'removeEmailFromList', 'showStatus',
        'loadReminders', 'loadSplitCounts', 'adjustSplitCounts',
        'extendThreadGroups', 'invalidateSplitListCache', 'renderEmailList',
        'setMode', 'showView', 'maybeRefillEmails', 'refillSuppressedIds',
        code,
    )(
        state, api, pushUndo, removeEmailFromList, () => {},
        () => {}, () => {}, () => {}, () => {}, () => {}, () => {},
        () => {}, () => {}, () => {}, refillSuppressedIds,
    );
    return { state, calls, remindEmail };
}

test('dd0d: remindEmail posts if-no-reply and removes optimistically', async () => {
    const h = makeHarness();
    await h.remindEmail('email-1', '2030-01-02T15:00:00.000Z', 'if-no-reply');
    assert.deepEqual(h.calls[0], {
        method: 'POST',
        path: '/emails/email-1/remind',
        body: { wake_at: '2030-01-02T15:00:00.000Z', mode: 'if-no-reply' },
    });
    assert.equal(h.state.emails.length, 0);
    assert.equal(h.state.undoStack.length, 1);
});

test('dd0d: remindEmail posts regardless mode', async () => {
    const h = makeHarness();
    await h.remindEmail('email-1', '2030-01-02T15:00:00.000Z', 'regardless');
    assert.equal(h.calls[0].body.mode, 'regardless');
    assert.equal(h.state.undoStack[0].reminder.mode, 'regardless');
});

test('dd0d: remindEmail restores the email and undo entry on failure', async () => {
    const h = makeHarness(() => Promise.reject(new Error('offline')));
    await h.remindEmail('email-1', '2030-01-02T15:00:00.000Z', 'if-no-reply');
    assert.equal(h.state.emails.length, 1);
    assert.equal(h.state.emails[0].id, 'email-1');
    assert.equal(h.state.undoStack.length, 0);
});

test('dd0d: Tab toggles the picker mode', () => {
    const state = { remindMode: 'if-no-reply' };
    const code = [
        extractFunction('function handleRemindPickerKey('),
        'return handleRemindPickerKey;',
    ].join('\n');
    // eslint-disable-next-line no-new-func
    const handle = new Function('state', 'renderRemindMode', 'closeRemindPicker', 'confirmRemindPicker', code)(
        state, () => {}, () => {}, () => {},
    );
    handle({ key: 'Tab', preventDefault() {} });
    assert.equal(state.remindMode, 'regardless');
});

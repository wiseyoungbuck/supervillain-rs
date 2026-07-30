// Behavioral tests for inbox calendar-invite chips (kata trbx).
//
// Extracts the REAL renderInviteChip function from both production bundles and
// exercises its runtime output. This mirrors tests/palette_test.cjs: no copied
// renderer logic, and the column-0 closing-brace convention matches routes.rs'
// js_fn_body helper.

const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

function extractFunction(src, declaration) {
    const start = src.indexOf(declaration);
    assert.notStrictEqual(start, -1, `${declaration} must exist`);
    const close = src.indexOf('\n}', start);
    assert.notStrictEqual(close, -1, `${declaration} must close with a column-0 brace`);
    return src.slice(start, close + 2);
}

function loadRenderer(relativePath) {
    const src = fs.readFileSync(path.join(__dirname, '..', relativePath), 'utf8');
    const code = extractFunction(src, 'function renderInviteChip(');
    // eslint-disable-next-line no-new-func
    return new Function(code + '\nreturn renderInviteChip;')();
}

for (const [surface, relativePath] of [
    ['desktop', 'static/app.js'],
    ['mobile', 'static/mobile/app.js'],
]) {
    test(`trbx: ${surface} renders every RSVP status`, () => {
        const renderInviteChip = loadRenderer(relativePath);
        for (const [status, label, cls] of [
            ['NEEDS-ACTION', 'Needs response', 'needs-action'],
            ['ACCEPTED', 'Accepted', 'accepted'],
            ['TENTATIVE', 'Tentative', 'tentative'],
            ['DECLINED', 'Declined', 'declined'],
            ['DELEGATED', 'Delegated', 'delegated'],
        ]) {
            const html = renderInviteChip({
                inviteMethod: 'REQUEST',
                isInviteToMe: true,
                inviteStatus: status,
                inviteIsUpdated: false,
            });
            assert.match(html, /📅/, `${surface} ${status} needs a calendar indicator`);
            assert.match(html, new RegExp(label), `${surface} ${status} needs its label`);
            assert.match(html, new RegExp(`email-invite--${cls}`), `${surface} ${status} needs its class`);
            assert.match(html, new RegExp(`aria-label="Calendar invite: ${label}"`));
        }
    });

    test(`trbx: ${surface} renders Updated ahead of stale RSVP status`, () => {
        const renderInviteChip = loadRenderer(relativePath);
        const html = renderInviteChip({
            inviteMethod: 'REQUEST',
            isInviteToMe: true,
            inviteStatus: 'ACCEPTED',
            inviteIsUpdated: true,
        });
        assert.match(html, /Updated/);
        assert.match(html, /email-invite--updated/);
        assert.doesNotMatch(html, />Accepted</);
    });

    test(`trbx: ${surface} hides non-invites`, () => {
        const renderInviteChip = loadRenderer(relativePath);
        const base = { isInviteToMe: true, inviteStatus: 'NEEDS-ACTION', inviteIsUpdated: false };
        assert.equal(renderInviteChip({ ...base, inviteMethod: 'REPLY' }), '');
        assert.equal(renderInviteChip({ ...base, inviteMethod: 'CANCEL' }), '');
        assert.equal(renderInviteChip({ ...base, inviteMethod: 'REQUEST', isInviteToMe: false }), '');
        assert.equal(renderInviteChip({}), '');
    });
}

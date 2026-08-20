// Behavioral tests for per-identity signatures (kata zqrn): switching the
// compose From identity swaps ONLY the signature block, preserving any
// user-edited body text.
//
// Extracts the REAL functions from both production bundles (column-0
// closing-brace convention, invite_chip_test.cjs style). The three shared
// helpers must stay byte-identical desktop↔mobile (1v8z one-file guardrail:
// mobile duplicates rather than imports).

const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const DESKTOP = fs.readFileSync(path.join(__dirname, '..', 'static', 'app.js'), 'utf8');
const MOBILE = fs.readFileSync(path.join(__dirname, '..', 'static', 'mobile', 'app.js'), 'utf8');

function extractFunction(src, declaration) {
    const start = src.indexOf(declaration);
    assert.notStrictEqual(start, -1, `${declaration} must exist`);
    const close = src.indexOf('\n}', start);
    assert.notStrictEqual(close, -1, `${declaration} must close with a column-0 brace`);
    return src.slice(start, close + 2);
}

const HELPERS = [
    'function sigBlock(',
    'function signatureForIdentity(',
    'function swapComposeSignature(',
];

function loadHelpers(src, state) {
    const code = HELPERS.map(d => extractFunction(src, d)).join('\n');
    // eslint-disable-next-line no-new-func
    return new Function(
        'state',
        code + '\nreturn { sigBlock, signatureForIdentity, swapComposeSignature };'
    )(state);
}

test('zqrn: signature helpers are byte-identical desktop <-> mobile', () => {
    for (const decl of HELPERS) {
        assert.equal(
            extractFunction(DESKTOP, decl),
            extractFunction(MOBILE, decl),
            `${decl} must stay byte-identical across surfaces — change both or neither`
        );
    }
});

test('zqrn: swapComposeSignature swaps only the signature block', () => {
    const { swapComposeSignature } = loadHelpers(DESKTOP, {});
    const body = 'Hello team,\nnumbers attached.\n\n-- \nOld Sig';
    assert.equal(
        swapComposeSignature(body, 'Old Sig', 'New Sig'),
        'Hello team,\nnumbers attached.\n\n-- \nNew Sig'
    );
});

test('zqrn: user-edited body text above the block is preserved through a swap', () => {
    const { swapComposeSignature } = loadHelpers(DESKTOP, {});
    const body = 'I typed a whole draft here.\nMore lines.\n\n-- \nAcct';
    const out = swapComposeSignature(body, 'Acct', 'Jane\nVP of Naps');
    assert.equal(out, 'I typed a whole draft here.\nMore lines.\n\n-- \nJane\nVP of Naps');
});

test('zqrn: a user-deleted or hand-edited signature block never gets clobbered', () => {
    const { swapComposeSignature } = loadHelpers(DESKTOP, {});
    // The tracked block is gone (user rewrote it) — the swap must be a no-op.
    const edited = 'Draft text\n\n-- \nmy own custom sign-off';
    assert.equal(swapComposeSignature(edited, 'Old Sig', 'New Sig'), edited);
});

test('zqrn: swapping from no signature appends; swapping to none removes the block', () => {
    const { swapComposeSignature } = loadHelpers(DESKTOP, {});
    assert.equal(
        swapComposeSignature('Just a draft', '', 'New Sig'),
        'Just a draft\n\n-- \nNew Sig'
    );
    assert.equal(
        swapComposeSignature('Just a draft\n\n-- \nOld Sig', 'Old Sig', ''),
        'Just a draft'
    );
});

test('zqrn: signatureForIdentity resolves per-identity, falls back to account, honors explicit empty', () => {
    const state = {
        currentAccount: {
            signature: 'Account-wide sig',
            identitySignatures: {
                'jane@example.com': 'Jane sig',
                'noreply@example.com': '',
            },
        },
    };
    const { signatureForIdentity } = loadHelpers(DESKTOP, state);
    assert.equal(signatureForIdentity('Jane@Example.com'), 'Jane sig');
    assert.equal(signatureForIdentity('other@example.com'), 'Account-wide sig');
    // An explicit empty entry means "this identity signs nothing" — it must
    // NOT fall back to the account signature.
    assert.equal(signatureForIdentity('noreply@example.com'), '');
    assert.equal(loadHelpers(DESKTOP, {}).signatureForIdentity('x@y.com'), '');
});

// ---- The observable compose-body behavior: From change swaps the signature.

function loadApply(state, els) {
    const code =
        HELPERS.map(d => extractFunction(DESKTOP, d)).join('\n') +
        '\n' +
        extractFunction(DESKTOP, 'function applyComposeSignatureForFrom(');
    // eslint-disable-next-line no-new-func
    return new Function(
        'state', 'els',
        code + '\nreturn applyComposeSignatureForFrom;'
    )(state, els);
}

function composeState(overrides = {}) {
    return {
        currentAccount: {
            signature: 'Acct sig',
            identitySignatures: { 'jane@work.com': 'Jane work sig' },
        },
        identities: [{ email: 'me@work.com' }, { email: 'jane@work.com' }],
        composeSignature: 'Acct sig',
        composeBaseline: '\n\n-- \nAcct sig',
        ...overrides,
    };
}

test('zqrn: From change swaps the prefilled signature in the compose body', () => {
    const state = composeState();
    const els = {
        composeFrom: { value: 'jane@work.com' },
        composeBody: { value: '\n\n-- \nAcct sig' },
    };
    const apply = loadApply(state, els);
    apply();
    assert.equal(els.composeBody.value, '\n\n-- \nJane work sig');
    assert.equal(state.composeSignature, 'Jane work sig');
    // Untouched compose: the dirty-check baseline must follow the swap so a
    // pristine body doesn't start autosaving as a draft.
    assert.equal(state.composeBaseline, '\n\n-- \nJane work sig');
});

test('zqrn: From change preserves typed text and does not move the baseline of a dirty body', () => {
    const state = composeState();
    const els = {
        composeFrom: { value: 'jane@work.com' },
        composeBody: { value: 'Typed something\n\n-- \nAcct sig' },
    };
    const apply = loadApply(state, els);
    apply();
    assert.equal(els.composeBody.value, 'Typed something\n\n-- \nJane work sig');
    assert.equal(
        state.composeBaseline, '\n\n-- \nAcct sig',
        'a dirty body must stay dirty — the baseline must not chase the swap'
    );
});

test('zqrn: switching back restores the account signature', () => {
    const state = composeState({ composeSignature: 'Jane work sig' });
    const els = {
        composeFrom: { value: 'me@work.com' },
        composeBody: { value: 'Hi\n\n-- \nJane work sig' },
    };
    loadApply(state, els)();
    assert.equal(els.composeBody.value, 'Hi\n\n-- \nAcct sig');
});

test('zqrn: perf — prefill resolution across 50 identities x 10KB signatures under 20ms', () => {
    // Budget per the plan table (CI-tolerant, rate_limit.rs discipline).
    const sig10k = 'S'.repeat(10 * 1024);
    const identitySignatures = {};
    const identities = [];
    for (let i = 0; i < 50; i++) {
        identitySignatures[`id${i}@example.com`] = `${sig10k}#${i}`;
        identities.push({ email: `id${i}@example.com` });
    }
    const state = { currentAccount: { signature: 'acct', identitySignatures }, identities };
    const { sigBlock, signatureForIdentity, swapComposeSignature } = loadHelpers(DESKTOP, state);

    const start = process.hrtime.bigint();
    let body = 'Draft text' + sigBlock(signatureForIdentity('id0@example.com'));
    let prev = signatureForIdentity('id0@example.com');
    for (let i = 1; i < 50; i++) {
        const next = signatureForIdentity(`id${i}@example.com`);
        body = swapComposeSignature(body, prev, next);
        prev = next;
    }
    const elapsedMs = Number(process.hrtime.bigint() - start) / 1e6;
    assert.ok(body.endsWith('#49'), 'the final identity signature must be in place');
    assert.ok(elapsedMs < 20, `50-identity prefill/swap took ${elapsedMs.toFixed(2)}ms, budget 20ms`);
});

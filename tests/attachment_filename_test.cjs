// Behavioral tests for the attachment upload filename contract (kata sbrd).
//
// Both browser bundles must turn File.name into the same ASCII-only RFC 5987
// extended value before assigning it to X-Filename.  This extracts and runs
// the REAL helper from each shipped bundle so Japanese, emoji, and accented
// Latin filenames cannot regress to raw, invalid HTTP header text.

const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const BUNDLES = [
    ['desktop', path.join(__dirname, '..', 'static', 'app.js')],
    ['mobile', path.join(__dirname, '..', 'static', 'mobile', 'app.js')],
];

function extractFunction(source, declaration) {
    const start = source.indexOf(declaration);
    assert.notStrictEqual(start, -1, `${declaration} must exist`);
    const close = source.indexOf('\n}', start);
    assert.notStrictEqual(close, -1, `${declaration} must close with a column-0 brace`);
    return source.slice(start, close + 2);
}

function loadEncoder(filename) {
    const source = fs.readFileSync(filename, 'utf8');
    const code = extractFunction(source, 'function encodeFilenameHeader(')
        + '\nreturn encodeFilenameHeader;';
    // eslint-disable-next-line no-new-func
    return new Function(code)();
}

const CASES = [
    ['添付資料.pdf', "UTF-8''%E6%B7%BB%E4%BB%98%E8%B3%87%E6%96%99.pdf"],
    ['launch-🚀.zip', "UTF-8''launch-%F0%9F%9A%80.zip"],
    ['résumé.pdf', "UTF-8''r%C3%A9sum%C3%A9.pdf"],
    ["director's cut (final).mov", "UTF-8''director%27s%20cut%20%28final%29.mov"],
];

for (const [bundle, filename] of BUNDLES) {
    test(`sbrd: ${bundle} RFC 5987-encodes non-ASCII upload filenames`, () => {
        const encodeFilenameHeader = loadEncoder(filename);
        for (const [name, expected] of CASES) {
            const wire = encodeFilenameHeader(name);
            assert.equal(wire, expected);
            assert.match(wire, /^[\x20-\x7e]+$/, 'X-Filename must contain ASCII only');
        }
    });
}

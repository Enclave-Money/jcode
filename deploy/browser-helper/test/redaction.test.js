'use strict';

// The helper handles a live credential in fill_and_submit. This is the
// source-level guard the plan promises: the fill and helper sources must not
// interpolate the credential-bearing variables into any log/write call. It is
// a blunt grep, deliberately — a blunt grep cannot be argued with in review.

const { test } = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const path = require('node:path');

function sourceOf(name) {
  return fs.readFileSync(path.join(__dirname, '..', name), 'utf8');
}

test('no credential variable is ever logged or written to stdout', () => {
  const bad = [];
  for (const file of ['helper.js', 'fill.js']) {
    const src = sourceOf(file);
    src.split('\n').forEach((line, i) => {
      const logs = /(?:console\.\w+|process\.(?:stdout|stderr)\.write|log)\s*\(/.test(line);
      if (!logs) return;
      // A credential reaches these lines only as creds.password / .username /
      // .totp or args.password / .username / .totp. None may appear on a log
      // line.
      if (/\b(?:creds|args)\.(?:password|username|totp)\b/.test(line)) {
        bad.push(`${file}:${i + 1}: ${line.trim()}`);
      }
    });
  }
  assert.deepEqual(bad, [], `credential-bearing log lines found:\n${bad.join('\n')}`);
});

test('fill_and_submit args are marked redacted where the command name is logged', () => {
  const src = sourceOf('helper.js');
  assert.match(src, /fill_and_submit.*\(redacted\)/, 'the dispatch log must special-case fill_and_submit');
});

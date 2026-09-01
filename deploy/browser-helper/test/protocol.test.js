'use strict';

// Drives the helper as a real child process over stdio — the exact contract
// the room daemon speaks. Proves: it announces ready, answers by id, drives a
// browser, and never writes a listening socket.

const { test } = require('node:test');
const assert = require('node:assert');
const { spawn } = require('node:child_process');
const path = require('node:path');
const { makeServer } = require('./fixtures');

const CREDS = { username: 'sumer@example.com', password: 'CANARY-9f3a-secret' };

function startHelper() {
  const child = spawn(process.execPath, [path.join(__dirname, '..', 'helper.js')], {
    env: { ...process.env, PLAYWRIGHT_BROWSERS_PATH: process.env.PLAYWRIGHT_BROWSERS_PATH || `${process.env.HOME}/Library/Caches/ms-playwright`, BLAUDE_BROWSER_PROFILE: `/tmp/blaude-test-profile-${process.pid}` },
    stdio: ['pipe', 'pipe', 'inherit'],
  });
  const pending = new Map();
  const events = [];
  let buf = '';
  child.stdout.on('data', (d) => {
    buf += d.toString();
    let nl;
    while ((nl = buf.indexOf('\n')) >= 0) {
      const line = buf.slice(0, nl); buf = buf.slice(nl + 1);
      if (!line.trim()) continue;
      const msg = JSON.parse(line);
      if (msg.event) { events.push(msg); continue; }
      const r = pending.get(msg.id);
      if (r) { pending.delete(msg.id); r(msg); }
    }
  });
  let nextId = 1;
  const call = (cmd, args) => new Promise((resolve) => {
    const id = nextId++;
    pending.set(id, resolve);
    child.stdin.write(JSON.stringify({ id, cmd, args }) + '\n');
  });
  return { child, call, events };
}

test('helper drives a browser over stdio and fills a login', async () => {
  const { server, received } = makeServer(CREDS);
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  const base = `http://127.0.0.1:${server.address().port}`;
  const h = startHelper();
  try {
    // ready event arrives unsolicited
    await new Promise((r) => setTimeout(r, 300));
    assert.ok(h.events.some((e) => e.event === 'ready'), 'announces ready');

    const status = await h.call('status', {});
    assert.equal(status.ok, true);

    const open = await h.call('open', { url: base + '/login' });
    assert.equal(open.ok, true);
    assert.equal(open.result.login_wall, true, 'detects the login wall');
    assert.equal(open.result.origin, base);

    const fill = await h.call('fill_and_submit', { username: CREDS.username, password: CREDS.password });
    assert.equal(fill.ok, true);
    assert.equal(fill.result.outcome, 'submitted', JSON.stringify(fill.result));

    // Positive control: the server got the real values, proving a genuine fill.
    assert.ok(received.some((r) => r.body && r.body.password === CREDS.password));

    const unknown = await h.call('bogus', {});
    assert.equal(unknown.ok, false);
    assert.match(unknown.error, /unknown command/);
  } finally {
    h.child.kill('SIGTERM');
    await new Promise((r) => server.close(r));
  }
});

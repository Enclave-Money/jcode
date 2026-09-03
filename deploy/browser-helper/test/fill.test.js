'use strict';

const { test, before, after } = require('node:test');
const assert = require('node:assert');
const { chromium } = require('playwright');
const { fillAndSubmit } = require('../fill');
const { loginHint } = require('../helper');
const { makeServer } = require('./fixtures');

const CREDS = { username: 'sumer@example.com', password: 'CANARY-9f3a-secret', totp: null };
const CREDS_TOTP = { username: 'sumer@example.com', password: 'CANARY-9f3a-secret', totp: '424242' };

let browser;
before(async () => { browser = await chromium.launch({ headless: true }); });
after(async () => { await browser.close(); });

async function withServer(creds, fn) {
  const { server, received } = makeServer(creds);
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  const base = `http://127.0.0.1:${server.address().port}`;
  const ctx = await browser.newContext();
  const page = await ctx.newPage();
  try { await fn({ page, base, received }); }
  finally { await ctx.close(); await new Promise((r) => server.close(r)); }
}

test('simple username+password login submits', async () => {
  await withServer({ ...CREDS }, async ({ page, base, received }) => {
    await page.goto(base + '/login');
    const res = await fillAndSubmit(page, CREDS);
    assert.equal(res.outcome, 'submitted', JSON.stringify(res));
    // Positive control: the server really received the right credential.
    const post = received.find((r) => r.path === '/login');
    assert.equal(post.body.username, CREDS.username);
    assert.equal(post.body.password, CREDS.password);
  });
});

test('username-first flow reaches the password step and submits', async () => {
  await withServer({ ...CREDS }, async ({ page, base }) => {
    await page.goto(base + '/login/username-first');
    const res = await fillAndSubmit(page, CREDS);
    assert.equal(res.outcome, 'submitted', JSON.stringify(res));
  });
});

test('login inside a same-origin iframe submits', async () => {
  await withServer({ ...CREDS }, async ({ page, base, received }) => {
    await page.goto(base + '/login/iframe');
    const res = await fillAndSubmit(page, CREDS);
    assert.equal(res.outcome, 'submitted', JSON.stringify(res));
    assert.ok(received.some((r) => r.path === '/login'));
  });
});

test('TOTP step is filled when the site asks', async () => {
  await withServer({ ...CREDS_TOTP }, async ({ page, base, received }) => {
    await page.goto(base + '/login');
    const res = await fillAndSubmit(page, CREDS_TOTP);
    assert.equal(res.outcome, 'submitted', JSON.stringify(res));
    assert.ok(received.some((r) => r.path === '/otp' && r.body.code === '424242'));
  });
});

test('wrong password stays on login and returns needs_human', async () => {
  await withServer({ ...CREDS }, async ({ page, base }) => {
    await page.goto(base + '/login');
    const res = await fillAndSubmit(page, { ...CREDS, password: 'wrong' });
    assert.equal(res.outcome, 'needs_human', JSON.stringify(res));
    assert.equal(res.reason, 'still_on_login');
    // Audit V2: a non-submitted outcome must leave no typed password in the
    // DOM for a later get_content/eval to read back.
    const leftover = await page.locator('input[type=password]').first().inputValue();
    assert.equal(leftover, '', 'password field must be scrubbed after a failed fill');
  });
});

test('a cross-origin hop mid-fill refuses instead of typing the password there', async () => {
  // Audit V3: username-first on origin A, whose "next" step 302s to origin B
  // (an open redirect, a federated IdP, or agent JS). The password belongs to
  // A and must never be typed into B.
  const { makeServer: mk } = require('./fixtures');
  const { server: evil, received: stolen } = mk({ ...CREDS });
  await new Promise((r) => evil.listen(0, '127.0.0.1', r));
  const evilBase = `http://127.0.0.1:${evil.address().port}`;
  try {
    const { server, received } = mk({ ...CREDS }, { hopTo: evilBase + '/login' });
    await new Promise((r) => server.listen(0, '127.0.0.1', r));
    const base = `http://127.0.0.1:${server.address().port}`;
    const ctx = await browser.newContext();
    const page = await ctx.newPage();
    try {
      await page.goto(base + '/login/username-first');
      const res = await fillAndSubmit(page, { ...CREDS, origin: base });
      assert.equal(res.outcome, 'origin_changed', JSON.stringify(res));
      // The other origin got nothing, and no password POST happened anywhere.
      assert.ok(!stolen.some((r) => r.body && r.body.password), 'no credential reached the hop target');
      assert.ok(!received.some((r) => r.body && r.body.password), 'no password was submitted at all');
    } finally { await ctx.close(); await new Promise((r) => server.close(r)); }
  } finally { await new Promise((r) => evil.close(r)); }
});

test('passkey-only page returns unsupported_auth', async () => {
  await withServer({ ...CREDS }, async ({ page, base }) => {
    await page.goto(base + '/login/passkey');
    const res = await fillAndSubmit(page, CREDS);
    assert.equal(res.outcome, 'unsupported_auth', JSON.stringify(res));
  });
});

test('captcha page returns needs_human with reason captcha', async () => {
  await withServer({ ...CREDS }, async ({ page, base }) => {
    await page.goto(base + '/login/captcha');
    const res = await fillAndSubmit(page, CREDS);
    assert.equal(res.outcome, 'needs_human', JSON.stringify(res));
    assert.equal(res.reason, 'captcha');
  });
});

test('loginHint flags a login wall and names the origin', async () => {
  await withServer({ ...CREDS }, async ({ page, base }) => {
    await page.goto(base + '/login');
    const hint = await loginHint(page);
    assert.equal(hint.login_wall, true);
    assert.equal(hint.origin, base);
  });
});

test('loginHint is quiet on an ordinary page', async () => {
  await withServer({ ...CREDS }, async ({ page, base }) => {
    await page.goto(base + '/app');
    const hint = await loginHint(page);
    assert.equal(hint.login_wall, false);
  });
});

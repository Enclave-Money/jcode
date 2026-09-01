#!/usr/bin/env node
'use strict';

// Harness-owned browser for a blaude room.
//
// A child of the room daemon, talking JSON-lines over stdin/stdout. It never
// opens a listening socket of any kind — stdio is the whole transport, which
// on a shared-localhost box is the only channel another room's user cannot
// reach. Playwright drives a headed Chromium on the room's X display, so a
// human watching the streamed screen sees exactly what the agent's browser
// does.
//
// Protocol:
//   in : {"id": <n>, "cmd": "<name>", "args": {...}}\n
//   out: {"id": <n>, "ok": true, "result": {...}}\n
//        {"id": <n>, "ok": false, "error": "<message>"}\n
//   unsolicited: {"event": "closed"}\n  when the browser goes away.
//
// The `fill_and_submit` command is the one whose args carry a secret. Its
// args are NEVER logged; see `log()` and the dispatch below.

const readline = require('readline');
const { chromium } = require('playwright');
const detect = require('./detect');
const { fillAndSubmit } = require('./fill');

let context = null;
let idleTimer = null;
const IDLE_MS = 15 * 60 * 1000;
const PROFILE_DIR = process.env.BLAUDE_BROWSER_PROFILE || '/tmp/blaude-browser-profile';

function log(...args) {
  // Diagnostics go to stderr (the daemon captures it); stdout is the protocol
  // channel only. Never called with command args.
  process.stderr.write('[browser-helper] ' + args.join(' ') + '\n');
}

function send(obj) {
  process.stdout.write(JSON.stringify(obj) + '\n');
}

function bumpIdle() {
  if (idleTimer) clearTimeout(idleTimer);
  idleTimer = setTimeout(async () => {
    log('idle timeout, closing');
    await shutdown();
    send({ event: 'closed', reason: 'idle' });
    process.exit(0);
  }, IDLE_MS);
}

async function ensureContext() {
  if (context) return context;
  context = await chromium.launchPersistentContext(PROFILE_DIR, {
    headless: false,
    viewport: { width: 1280, height: 720 },
    args: ['--no-first-run', '--no-default-browser-check', '--disable-features=Translate'],
  });
  context.on('close', () => {
    context = null;
    send({ event: 'closed', reason: 'browser_exit' });
  });
  if (context.pages().length === 0) await context.newPage();
  return context;
}

function activePage(ctx) {
  const pages = ctx.pages();
  return pages[pages.length - 1] || null;
}

async function loginHint(page) {
  // The detection hint attached to every navigating result. Reads structure,
  // never values.
  const url = page.url();
  let dom = { isLoginWall: false, passkeyOnly: false, captcha: false, usernameFirst: false, hasPassword: false };
  try {
    dom = await page.evaluate(detect.DOM_PROBE_SOURCE);
  } catch { /* page navigating; the URL heuristic still stands */ }
  return {
    login_wall: dom.isLoginWall || detect.urlLooksLikeLogin(url),
    passkey_only: dom.passkeyOnly,
    captcha: dom.captcha,
    origin: detect.originOf(url),
    url,
  };
}

// Command handlers. Each returns the `result` object.
const commands = {
  async status() {
    return { ready: !!context, backend: 'room_playwright', browser: 'chromium' };
  },
  async open(args) {
    const ctx = await ensureContext();
    const page = args.new_tab ? await ctx.newPage() : activePage(ctx);
    await page.goto(args.url, { waitUntil: 'domcontentloaded', timeout: args.timeout_ms || 30000 });
    return { ...(await loginHint(page)) };
  },
  async click(args) {
    const page = activePage(await ensureContext());
    if (args.selector) await page.locator(args.selector).first().click({ timeout: args.timeout_ms || 5000 });
    else if (args.x != null) await page.mouse.click(args.x, args.y);
    return { ...(await loginHint(page)) };
  },
  async type(args) {
    const page = activePage(await ensureContext());
    const loc = page.locator(args.selector).first();
    if (args.clear) await loc.fill('');
    await loc.type(args.text, { delay: 10 });
    if (args.submit) await loc.press('Enter');
    return { ok: true };
  },
  async press(args) {
    const page = activePage(await ensureContext());
    await page.keyboard.press(args.key);
    return { ...(await loginHint(page)) };
  },
  async wait(args) {
    const page = activePage(await ensureContext());
    if (args.selector) await page.locator(args.selector).first().waitFor({ timeout: args.timeout_ms || 10000 });
    else await page.waitForLoadState('networkidle', { timeout: args.timeout_ms || 10000 }).catch(() => {});
    return { ...(await loginHint(page)) };
  },
  async screenshot() {
    const page = activePage(await ensureContext());
    const buf = await page.screenshot({ type: 'jpeg', quality: 60 });
    return { format: 'jpeg', base64: buf.toString('base64') };
  },
  async get_content() {
    const page = activePage(await ensureContext());
    return { url: page.url(), title: await page.title(), text: (await page.locator('body').innerText().catch(() => '')).slice(0, 20000) };
  },
  async eval(args) {
    const page = activePage(await ensureContext());
    // Arbitrary agent script, same capability the Firefox bridge exposes.
    const value = await page.evaluate(args.script);
    return { value };
  },
  async scroll(args) {
    const page = activePage(await ensureContext());
    const dy = args.position === 'top' ? -1e6 : args.position === 'bottom' ? 1e6 : (args.dy || 400);
    await page.mouse.wheel(0, dy);
    return { ok: true };
  },
  async list_tabs() {
    const ctx = await ensureContext();
    return { tabs: ctx.pages().map((p, i) => ({ tab_id: i, url: p.url() })) };
  },
  async new_tab(args) {
    const ctx = await ensureContext();
    const page = await ctx.newPage();
    if (args.url) await page.goto(args.url, { waitUntil: 'domcontentloaded' });
    return { tab_id: ctx.pages().length - 1, ...(await loginHint(page)) };
  },
  async detect_login(_args) {
    const page = activePage(await ensureContext());
    return { ...(await loginHint(page)) };
  },
  // The privileged action. Its args carry the credential and are never logged.
  async fill_and_submit(args) {
    const page = activePage(await ensureContext());
    const creds = { username: args.username, password: args.password, totp: args.totp };
    return await fillAndSubmit(page, creds);
  },
  async close() {
    await shutdown();
    return { ok: true };
  },
};

async function shutdown() {
  if (idleTimer) clearTimeout(idleTimer);
  if (context) {
    const c = context;
    context = null;
    await c.close().catch(() => {});
  }
}

async function handle(line) {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return; // not protocol; ignore
  }
  const { id, cmd, args = {} } = msg;
  const handler = commands[cmd];
  if (!handler) {
    send({ id, ok: false, error: `unknown command: ${cmd}` });
    return;
  }
  // Log the command name only — never args, so a secret in fill_and_submit
  // cannot reach even stderr.
  log('cmd', cmd, cmd === 'fill_and_submit' ? '(redacted)' : JSON.stringify(args).slice(0, 200));
  bumpIdle();
  try {
    const result = await handler(args);
    send({ id, ok: true, result });
  } catch (err) {
    send({ id, ok: false, error: String((err && err.message) || err) });
  }
}

function main() {
  const rl = readline.createInterface({ input: process.stdin });
  rl.on('line', (line) => { handle(line); });
  rl.on('close', async () => { await shutdown(); process.exit(0); });
  process.on('SIGTERM', async () => { await shutdown(); process.exit(0); });
  bumpIdle();
  send({ event: 'ready' });
}

if (require.main === module) main();
module.exports = { commands, loginHint };

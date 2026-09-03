'use strict';

// The fill-and-submit state machine.
//
// One call does the whole thing: username → (maybe submit, wait for password)
// → password → (maybe TOTP) → submit → classify. Credentials live only in the
// arguments and locals here; nothing in this file logs a value, and the helper
// build lints this source for exactly that.
//
// It is deliberately conservative about "submitted": it declares success only
// when the password field is gone AND the URL left the login path, because a
// wrong-password reload also clears the fields for a moment.

const USER_SELECTORS = [
  'input[autocomplete=username]',
  'input[autocomplete=email]',
  'input[type=email]',
  'input[name=username]',
  'input[name=email]',
  'input[id=username]',
  'input[id=email]',
  'input[name*=user i]',
];
const PASSWORD_SELECTOR = 'input[type=password]:not([aria-hidden=true])';
const TOTP_SELECTORS = [
  'input[autocomplete=one-time-code]',
  'input[name*=otp i]',
  'input[name*=totp i]',
  'input[name*=code i]',
  'input[id*=otp i]',
  'input[inputmode=numeric]',
];
const SUBMIT_SELECTOR = 'button[type=submit], input[type=submit]';

// Every frame worth searching: frames ON THE APPROVED ORIGIN, nothing else.
// Cross-origin frames are skipped — Playwright can reach into them, but a
// login form spanning a truly foreign origin is the SSO case, out of scope.
//
// `origin` is the origin the human approved. Keying on the CURRENT main
// frame instead was an audit finding (V3): a username-first flow that hopped
// cross-origin (open redirect, federated IdP, agent-scripted location change)
// made the new page "same origin" by definition, and the password was typed
// wherever the browser had landed. With the approved origin pinned, a hop
// leaves nothing to search and the fill refuses instead of following.
function searchFrames(page, origin) {
  const target = origin || safeOrigin(page.mainFrame().url());
  return page.frames().filter((f) => {
    const o = safeOrigin(f.url());
    return o !== null && o === target;
  });
}

function safeOrigin(url) {
  try { return new URL(url).origin; } catch { return null; }
}

async function firstVisible(frames, selectors) {
  const list = Array.isArray(selectors) ? selectors : [selectors];
  for (const frame of frames) {
    for (const sel of list) {
      const loc = frame.locator(sel).first();
      try {
        if (await loc.isVisible({ timeout: 200 })) return loc;
      } catch { /* frame detached mid-search; skip */ }
    }
  }
  return null;
}

async function detectCaptcha(page) {
  for (const frame of page.frames()) {
    const u = frame.url() || '';
    if (/recaptcha|hcaptcha|turnstile/i.test(u)) return true;
  }
  try {
    // Presence, not visibility: a captcha container is often a zero-size
    // placeholder until its script inflates it, and either way its mere
    // presence on a login page means an automated submit will be challenged.
    return (await page.locator('.g-recaptcha, .h-captcha, .cf-turnstile, [data-sitekey]').count()) > 0;
  } catch { return false; }
}

async function looksPasskeyOnly(page) {
  const frames = searchFrames(page);
  const pw = await firstVisible(frames, PASSWORD_SELECTOR);
  const user = await firstVisible(frames, USER_SELECTORS);
  if (pw || user) return false;
  try {
    return await page.getByText(/passkey|security key|sign in with a passkey/i)
      .first().isVisible({ timeout: 200 });
  } catch { return false; }
}

// Clear any password field on every path that is not a submit, so a captured
// frame or a lingering DOM never contains typed characters (they are masked,
// but the value is in the DOM). ALL frames, not just approved ones — the scrub
// must reach a field wherever it is. Best-effort.
async function scrubPasswords(page) {
  for (const frame of page.frames()) {
    try { await frame.locator(PASSWORD_SELECTOR).evaluateAll((els) => els.forEach((e) => { e.value = ''; })); }
    catch { /* ignore */ }
  }
}

async function shot(page) {
  try {
    await scrubPasswords(page);
    const buf = await page.screenshot({ type: 'jpeg', quality: 50, fullPage: false });
    if (buf.length <= 200 * 1024) return buf.toString('base64');
  } catch { /* ignore */ }
  return null;
}

const { urlLooksLikeLogin } = require('./detect');

// creds: { username, password, totp?, origin? }. `origin` is the approved
// origin; every credential keystroke re-checks the page is still ON it, so a
// hop between finding a field and typing into it refuses rather than follows
// (audit V3 — the username-first wait is exactly where federated flows and
// open redirects move the page). deps lets tests inject fakes for the clock;
// production passes nothing.
async function fillAndSubmit(page, creds, deps = {}) {
  const sleep = deps.sleep || ((ms) => new Promise((r) => setTimeout(r, ms)));
  const approved = creds.origin || safeOrigin(page.url());
  const startUrl = page.url();
  const offOrigin = () => safeOrigin(page.mainFrame().url()) !== approved;
  const refuseHop = async () => {
    await scrubPasswords(page);
    return { outcome: 'origin_changed' };
  };

  if (offOrigin()) return await refuseHop();
  let frames = searchFrames(page, approved);
  const userLoc = await firstVisible(frames, USER_SELECTORS);
  let passLoc = await firstVisible(frames, PASSWORD_SELECTOR);

  if (!userLoc && !passLoc) {
    if (await looksPasskeyOnly(page)) return { outcome: 'unsupported_auth' };
    return { outcome: 'needs_human', reason: 'no_login_fields', screenshot: await shot(page) };
  }

  // Username, then advance a username-first flow to the password step.
  if (userLoc) {
    if (offOrigin()) return await refuseHop();
    await userLoc.fill(creds.username);
    if (!passLoc) {
      const submit = await firstVisible(frames, SUBMIT_SELECTOR);
      if (submit) await submit.click().catch(() => {});
      else await userLoc.press('Enter').catch(() => {});
      // Wait up to 8s across up to 2 hops for the password field to appear —
      // only ever on the approved origin.
      for (let i = 0; i < 16 && !passLoc; i++) {
        await sleep(500);
        frames = searchFrames(page, approved);
        passLoc = await firstVisible(frames, PASSWORD_SELECTOR);
      }
    }
  }

  if (!passLoc) {
    if (offOrigin()) return await refuseHop();
    if (await detectCaptcha(page)) return { outcome: 'needs_human', reason: 'captcha', screenshot: await shot(page) };
    return { outcome: 'needs_human', reason: 'no_password_field', screenshot: await shot(page) };
  }

  // The last word before the secret: the page may have moved between finding
  // the field and typing into it, and a Playwright locator re-resolves against
  // whatever document its frame holds NOW.
  if (offOrigin()) return await refuseHop();
  await passLoc.fill(creds.password);

  // Submit the password.
  const submit2 = await firstVisible(searchFrames(page, approved), SUBMIT_SELECTOR);
  if (submit2) await submit2.click().catch(() => {});
  else await passLoc.press('Enter').catch(() => {});

  // TOTP, if the site asks within 8s and we hold one.
  if (creds.totp) {
    for (let i = 0; i < 16; i++) {
      await sleep(500);
      const otp = await firstVisible(searchFrames(page, approved), TOTP_SELECTORS);
      if (otp) {
        if (offOrigin()) return await refuseHop();
        await otp.fill(creds.totp);
        const s = await firstVisible(searchFrames(page, approved), SUBMIT_SELECTOR);
        if (s) await s.click().catch(() => {});
        else await otp.press('Enter').catch(() => {});
        break;
      }
      // A 2FA prompt we can't answer (SMS/push) — bail to the human.
      if (i === 6 && await detectCaptcha(page)) break;
    }
  }

  // Settle, then classify.
  await page.waitForLoadState('networkidle', { timeout: 8000 }).catch(() => {});
  await sleep(300);

  if (await detectCaptcha(page)) {
    return { outcome: 'needs_human', reason: 'captcha', screenshot: await shot(page) };
  }
  const stillPassword = await firstVisible(searchFrames(page, approved), PASSWORD_SELECTOR);
  const stillUser = await firstVisible(searchFrames(page, approved), USER_SELECTORS);
  const nowUrl = page.url();
  // The main frame leaving the login path is the clearest success signal, but
  // a login inside an iframe succeeds without the top URL ever changing — so
  // "no login form remains anywhere" also counts. A wrong password re-renders
  // the form, so a lingering password field is what rules success out.
  const mainLeftLogin = !urlLooksLikeLogin(nowUrl) || nowUrl !== startUrl;
  if (!stillPassword && (mainLeftLogin || !stillUser)) return { outcome: 'submitted' };

  // Still on a login-shaped page: a wrong credential, an unmet 2FA, or a
  // silent block. Hand it to the human rather than guess or retry — with no
  // typed password left sitting in the DOM (shot() scrubs, but scrub even if
  // the screenshot fails).
  const screenshot = await shot(page);
  await scrubPasswords(page);
  return { outcome: 'needs_human', reason: 'still_on_login', screenshot };
}

module.exports = { fillAndSubmit, searchFrames, safeOrigin, USER_SELECTORS, PASSWORD_SELECTOR, TOTP_SELECTORS };

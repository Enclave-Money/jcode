'use strict';

// Login-wall detection.
//
// Two layers, kept separate so the cheap one is unit-testable without a
// browser: a URL heuristic that is a pure function of a string, and a DOM
// heuristic that runs inside the page. The daemon never auto-fires a fill on
// this — detection only sets a hint the agent acts on — so a false positive
// costs a wasted `fill_login` call, never a wrong credential.

const LOGIN_PATH = /(?:^|\/)(?:login|log-in|signin|sign-in|sign_in|auth|sso|session|account\/login|oauth|authorize)(?:\/|$|\?)/i;

// A pure function of the URL. Exported on its own so it can be tested with no
// browser and no network.
function urlLooksLikeLogin(url) {
  if (typeof url !== 'string' || url.length === 0) return false;
  let path;
  try {
    path = new URL(url).pathname + (new URL(url).search || '');
  } catch {
    path = url; // relative or malformed: match against the raw string
  }
  return LOGIN_PATH.test(path);
}

// The origin of a URL (scheme://host[:port]), or null if it will not parse.
// The index and the allow list key on this exact shape.
function originOf(url) {
  try {
    return new URL(url).origin;
  } catch {
    return null;
  }
}

// The in-page half, as a STRING of source so the helper can hand it to
// page.evaluate in every frame. Kept as source (not a live function) because
// it must run in the page's realm, not the helper's. Returns a small verdict
// object; it reads the DOM but never its values.
//
// Exposed as a string constant so the fixture tests can also eval it in a
// jsdom/page context and assert on the same code the helper ships.
const DOM_PROBE_SOURCE = `(() => {
  const visible = (el) => {
    if (!el) return false;
    const r = el.getBoundingClientRect();
    if (r.width === 0 && r.height === 0) return false;
    const s = getComputedStyle(el);
    return s.visibility !== 'hidden' && s.display !== 'none';
  };
  const passwords = Array.from(document.querySelectorAll('input[type=password]')).filter(visible);
  const userSel = 'input[type=email], input[autocomplete=username], input[autocomplete=email], input[name=username], input[name=email], input[id=username], input[id=email]';
  const users = Array.from(document.querySelectorAll(userSel)).filter(visible);
  const submits = Array.from(document.querySelectorAll('button[type=submit], input[type=submit], button')).filter(visible);
  // WebAuthn/passkey: a page that offers only a passkey button and no password.
  const passkeyHint = Array.from(document.querySelectorAll('button, a')).some((b) =>
    /passkey|security key|use your (phone|device)|sign in with a passkey/i.test(b.textContent || ''));
  const captcha = !!document.querySelector(
    'iframe[src*="recaptcha"], iframe[src*="hcaptcha"], iframe[src*="turnstile"], .g-recaptcha, [data-sitekey]');
  const hasPassword = passwords.length > 0;
  // Username-first: a lone username field with a submit and no password yet.
  const usernameFirst = !hasPassword && users.length > 0 && submits.length > 0;
  return {
    hasPassword,
    usernameFirst,
    passkeyOnly: passkeyHint && !hasPassword && users.length === 0,
    captcha,
    // A login wall is a password field, or a username-first step.
    isLoginWall: hasPassword || usernameFirst,
  };
})()`;

module.exports = { urlLooksLikeLogin, originOf, DOM_PROBE_SOURCE, LOGIN_PATH };

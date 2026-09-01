'use strict';

// A tiny login-server for the fill/detection tests. Serves a handful of login
// shapes and records what it received, so a test can prove a fill actually
// submitted the right values WITHOUT the helper ever reporting them (the
// positive control the canary test needs).

const http = require('http');

function page(body) {
  return `<!doctype html><html><head><meta charset=utf-8></head><body>${body}</body></html>`;
}

// origin session store: set once a correct credential posts.
function makeServer(creds) {
  const received = [];
  const server = http.createServer((req, res) => {
    const url = new URL(req.url, 'http://localhost');
    if (req.method === 'POST') {
      let data = '';
      req.on('data', (c) => (data += c));
      req.on('end', () => {
        const params = new URLSearchParams(data);
        received.push({ path: url.pathname, body: Object.fromEntries(params) });
        // Simple flow: /login accepts username+password; if TOTP required and
        // missing, redirect to /otp; else land on /app.
        if (url.pathname === '/login') {
          const ok = params.get('username') === creds.username && params.get('password') === creds.password;
          if (!ok) { res.writeHead(200); return res.end(page('<p>Wrong password</p>' + loginForm())); }
          if (creds.totp) { res.writeHead(302, { Location: '/otp' }); return res.end(); }
          res.writeHead(302, { Location: '/app' }); return res.end();
        }
        if (url.pathname === '/otp') {
          const ok = params.get('code') === creds.totp;
          if (!ok) { res.writeHead(200); return res.end(page('<p>Bad code</p>' + otpForm())); }
          res.writeHead(302, { Location: '/app' }); return res.end();
        }
        if (url.pathname === '/u') {
          // username-first step: stash and show password page
          res.writeHead(200); return res.end(page(passwordForm(params.get('username') || '')));
        }
        if (url.pathname === '/blocked') {
          // A captcha the automation cannot solve: the POST never succeeds,
          // the login page comes back. This is what a v2 challenge does.
          res.writeHead(200); return res.end(page(captchaPage()));
        }
        res.writeHead(404); return res.end('no');
      });
      return;
    }
    // GET
    if (url.pathname === '/app') { res.writeHead(200); return res.end(page('<h1>Signed in</h1>')); }
    if (url.pathname === '/otp') { res.writeHead(200); return res.end(page(otpForm())); }
    if (url.pathname === '/login/username-first') { res.writeHead(200); return res.end(page(usernameFirstForm())); }
    if (url.pathname === '/login/iframe') {
      res.writeHead(200);
      return res.end(page(`<iframe src="/login" style="width:400px;height:300px"></iframe>`));
    }
    if (url.pathname === '/login/passkey') { res.writeHead(200); return res.end(page(passkeyPage())); }
    if (url.pathname === '/login/captcha') { res.writeHead(200); return res.end(page(captchaPage())); }
    if (url.pathname === '/login' || url.pathname === '/') { res.writeHead(200); return res.end(page(loginForm())); }
    res.writeHead(404); res.end('no');
  });
  return { server, received };
}

function loginForm() {
  return `<form method=post action="/login">
    <input name=username autocomplete=username>
    <input name=password type=password autocomplete=current-password>
    <button type=submit>Sign in</button></form>`;
}
function passwordForm(user) {
  return `<form method=post action="/login">
    <input name=username type=hidden value="${user}">
    <input name=password type=password autocomplete=current-password>
    <button type=submit>Continue</button></form>`;
}
function usernameFirstForm() {
  return `<form method=post action="/u">
    <input name=username autocomplete=username>
    <button type=submit>Next</button></form>`;
}
function otpForm() {
  return `<form method=post action="/otp">
    <input name=code autocomplete=one-time-code inputmode=numeric>
    <button type=submit>Verify</button></form>`;
}
function passkeyPage() {
  return `<div><button>Sign in with a passkey</button></div>`;
}
function captchaPage() {
  // Form posts to /blocked, which never lets the login through — the automated
  // submit is defeated by the challenge, exactly as a real v2 captcha does.
  return `<form method=post action="/blocked">
    <input name=username autocomplete=username>
    <input name=password type=password autocomplete=current-password>
    <button type=submit>Sign in</button></form>
    <div class="g-recaptcha" data-sitekey="test"></div>`;
}

module.exports = { makeServer };

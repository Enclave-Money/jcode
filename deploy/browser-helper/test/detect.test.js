'use strict';

const { test } = require('node:test');
const assert = require('node:assert');
const { urlLooksLikeLogin, originOf } = require('../detect');

test('urlLooksLikeLogin matches common login routes', () => {
  for (const u of [
    'https://vercel.com/login',
    'https://x.com/i/flow/login',
    'https://accounts.google.com/signin/v2',
    'https://github.com/sign_in',
    'https://app.example.com/auth/callback',
    'https://id.example.com/authorize?client_id=1',
    '/session/new',
  ]) {
    assert.ok(urlLooksLikeLogin(u), `should match: ${u}`);
  }
});

test('urlLooksLikeLogin does not match ordinary pages', () => {
  for (const u of [
    'https://vercel.com/dashboard',
    'https://example.com/blog/logistics', // "log" substring must not trip it
    'https://example.com/products',
    'https://example.com/',
  ]) {
    assert.ok(!urlLooksLikeLogin(u), `should not match: ${u}`);
  }
});

test('originOf returns scheme://host[:port] or null', () => {
  assert.equal(originOf('https://vercel.com/login?x=1'), 'https://vercel.com');
  assert.equal(originOf('http://localhost:3000/app'), 'http://localhost:3000');
  assert.equal(originOf('not a url'), null);
});

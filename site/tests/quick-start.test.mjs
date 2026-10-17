import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const SITE = fileURLToPath(new URL('..', import.meta.url));
const INSTALL = 'curl -fsSL https://raw.githubusercontent.com/rzorzal/clusia/main/install.sh | bash';
const INSTALL_BLOCK = `<div class="install"><code data-copy tabindex="0" role="region" aria-label="Install command">${INSTALL}</code><button class="copy" aria-label="Copy install command">Copy</button></div>`;

const html = readFileSync(SITE + 'quick-start.html', 'utf8');
const main = html.slice(html.indexOf('<main'), html.indexOf('</main>'));
const sections = main.split('<section').slice(1).map((s) => '<section' + s.slice(0, s.indexOf('</section>')));

test('quick-start.html is the Quick start page, current in the nav, with the install block first', () => {
  assert.match(html, /<title>Quick start — Clúsia<\/title>/);
  assert.match(main, /<h1[^>]*>Quick start<\/h1>/);
  assert.match(html, /<a href="quick-start\.html" aria-current="page">Quick start<\/a>/);
  assert.ok(main.includes('Not installed yet?'));
  assert.ok(main.indexOf(INSTALL_BLOCK) < main.indexOf('<ol class="steps">'));
});

test('the first steps are the six README steps, in order', () => {
  const ol = main.match(/<ol class="steps">([\s\S]*?)<\/ol>/)[1];
  const items = [...ol.matchAll(/<li>([\s\S]*?)<\/li>/g)].map((m) => m[1]);
  assert.equal(items.length, 6);
  const order = ['allow notifications', '<code>gh</code>', 'first run', 'clusia open octo/hello#7', 'Shift-click', 'Finalize review'];
  order.forEach((needle, i) => assert.ok(items[i].toLowerCase().includes(needle.toLowerCase()), `${i + 1}: ${needle}`));
  assert.ok(items[1].includes('clusia auth login'));
});

test('the first run is shown as a screenshot pair that swaps by scheme', () => {
  assert.match(main, /<img class="shot-light" src="assets\/sp2-m5-first-run-both-light\.png" width="2560" height="1600" alt="[^"]+"/);
  assert.match(main, /<img class="shot-dark" src="assets\/sp2-m5-first-run-both-dark\.png" width="2560" height="1600" alt="[^"]+"/);
  assert.equal([...main.matchAll(/<img\b[^>]*assets\/sp[^>]*>/g)].length, 2);
});

test('everyday commands is a table of the nine README commands on demo data', () => {
  const table = main.match(/<table>[\s\S]*?<\/table>/)[0];
  const rows = [...table.matchAll(/<tr><td>([\s\S]*?)<\/td><td>([\s\S]*?)<\/td><\/tr>/g)];
  assert.equal(rows.length, 9);
  const cmds = ['auth status', 'prs', 'open', 'review comment', 'review publish', 'ask', 'check', 'diagrams', 'revalidate'];
  cmds.forEach((c, i) => assert.ok(rows[i][1].includes(`clusia ${c}`), c));
  assert.ok(!/owner\/repo/.test(table));
  assert.ok(table.includes('octo/hello#7'));
  assert.ok(!main.includes('rzorzal/') || !/rzorzal\/(?!clusia)/.test(main));
});

test('nothing holding the install block waits for a reveal', () => {
  for (const s of sections) if (s.includes('class="install"')) assert.ok(!/class="[^"]*\breveal\b/.test(s.slice(0, s.indexOf('>') + 1)));
});

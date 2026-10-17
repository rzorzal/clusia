import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const SITE = fileURLToPath(new URL('..', import.meta.url));
const INSTALL = 'curl -fsSL https://raw.githubusercontent.com/rzorzal/clusia/main/install.sh | bash';
const INSTALL_BLOCK = `<div class="install"><code data-copy tabindex="0" role="region" aria-label="Install command">${INSTALL}</code><button class="copy" aria-label="Copy install command">Copy</button></div>`;

const html = readFileSync(SITE + 'features.html', 'utf8');
const css = readFileSync(SITE + 'css/site.css', 'utf8');
const main = html.slice(html.indexOf('<main'), html.indexOf('</main>'));

const FEATURES = [
  ['Review in a local worktree', 'sp1-m5b-review'],
  ['Rich comments with a preview of what GitHub will show', 'sp1-m5c-composer'],
  ['Drafts that follow the code: revalidation after new commits', 'sp2-m4-revalidated'],
  ['One review, published when you are ready', 'sp2-m4-banner'],
  ['A chat with your own agent per review', 'sp2-m1-agent-chat'],
  ['It reads, you decide: the permission modal', 'sp2-m2-permission'],
  ['Security check and audit', 'sp2-m3-security'],
  ['Diagrams of the change', 'sp3-m1-diagrams'],
  ['Always at hand in the menu bar', 'sp1-tray-readable'],
];
const sections = main.split('<section').slice(1).map((s) => '<section' + s.slice(0, s.indexOf('</section>')));
const sectionWith = (h) => sections.find((s) => s.includes(`>${h}</h2>`));

test('features.html is the Features page with the install block up front', () => {
  assert.match(html, /<title>Features — Clúsia<\/title>/);
  assert.match(main, /<h1[^>]*>Features<\/h1>/);
  assert.ok(main.includes(INSTALL_BLOCK));
});

test('each feature is an h2 with its screenshot pair, swapping by scheme', () => {
  for (const [heading, pair] of FEATURES) {
    const s = sectionWith(heading);
    assert.ok(s, heading);
    assert.match(s, new RegExp(`<img class="shot-light" src="assets/${pair}-light\\.png"`), pair);
    assert.match(s, new RegExp(`<img class="shot-dark" src="assets/${pair}-dark\\.png"`), pair);
    assert.match(s, /<p>[^<]+<\/p>/);
  }
  assert.equal([...main.matchAll(/<img\b[^>]*assets\/sp[^>]*>/g)].length, FEATURES.length * 2, 'no other screenshot');
});

test('the CLI is a terminal block of demo commands, not an image', () => {
  const s = sectionWith('The <code>clusia</code> CLI');
  assert.ok(s);
  assert.ok(!/<img\b/.test(s));
  for (const c of ['clusia prs', 'clusia open octo/hello#7', 'clusia review comment octo/hello#7 src/lib.rs:42 &quot;Is the lock needed?&quot;', 'clusia review publish octo/hello#7 --verdict approve']) {
    assert.ok(s.includes(c), c);
  }
});

test('nothing holding the install block waits for a reveal', () => {
  for (const s of sections) if (s.includes('class="install"')) assert.ok(!/class="[^"]*\breveal\b/.test(s.slice(0, s.indexOf('>') + 1)));
});

test('features alternate sides from 900 px up and stack below', () => {
  const at = css.indexOf('@media (min-width: 900px)');
  assert.ok(at > 0);
  assert.match(css.slice(at), /\.feature:nth-of-type\(odd\)/);
  assert.match(css, /\.card-wide\s*{/);
});

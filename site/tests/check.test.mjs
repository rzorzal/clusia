import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { checkDir } from '../tools/check.mjs';

const CHECK = fileURLToPath(new URL('../tools/check.mjs', import.meta.url));
const INSTALL = 'curl -fsSL https://raw.githubusercontent.com/rzorzal/clusia/main/install.sh | bash';

const NAV = `<nav>
  <a href="index.html">Home</a> <a href="features.html">Features</a>
  <a href="screens.html">Screens</a> <a href="install.html">Install</a>
  <a href="quick-start.html">Quick start</a>
  <a href="https://github.com/rzorzal/clusia">GitHub</a>
</nav>`;
const COPY = `<div class="install"><code data-copy>${INSTALL}</code></div>`;

const page = ({ head = '', body = '', copy = COPY } = {}) =>
  `<!doctype html><html><head><title>t</title>${head}</head><body>${NAV}${body}${copy}</body></html>`;

function fixture(files) {
  const dir = mkdtempSync(join(tmpdir(), 'site-check-'));
  for (const [name, html] of Object.entries(files)) writeFileSync(join(dir, name), html);
  return dir;
}

const ALL = Object.fromEntries(
  ['index.html', 'features.html', 'screens.html', 'install.html', 'quick-start.html'].map((n) => [n, page()]),
);

test('a clean fixture with all five pages passes with no output', () => {
  const dir = fixture({ ...ALL, 'index.html': page({ body: '<h1 id="top">x</h1><a href="#top">up</a>' }) });
  assert.deepEqual(checkDir(dir), []);
  const run = spawnSync('node', [CHECK, dir], { encoding: 'utf8' });
  assert.equal(run.status, 0);
});

test('a missing page fails, and so does every link to it', () => {
  const { 'features.html': _gone, ...rest } = ALL;
  const dir = fixture(rest);
  const problems = checkDir(dir);
  assert.ok(problems.some((p) => p.includes('features.html')));
  const run = spawnSync('node', [CHECK, dir], { encoding: 'utf8' });
  assert.equal(run.status, 1);
});

test('an absolute href fails', () => {
  const dir = fixture({ ...ALL, 'index.html': page({ head: '<link rel="stylesheet" href="/x.css">' }) });
  const run = spawnSync('node', [CHECK, dir], { encoding: 'utf8' });
  assert.equal(run.status, 1);
  assert.match(run.stdout, /index\.html.*\/x\.css/);
});

test('an img without alt fails', () => {
  const dir = fixture({ ...ALL, 'index.html': page({ body: '<img src="a.png" width="1" height="1">' }) });
  writeFileSync(join(dir, 'a.png'), '');
  assert.ok(checkDir(dir).some((p) => /alt/.test(p)));
});

test('an img without width and height fails', () => {
  const dir = fixture({ ...ALL, 'index.html': page({ body: '<img src="a.png" alt="a">' }) });
  writeFileSync(join(dir, 'a.png'), '');
  assert.ok(checkDir(dir).some((p) => /width/.test(p)));
});

test('a non-demo owner in owner/repo#N fails, a demo owner passes', () => {
  const bad = fixture({ ...ALL, 'index.html': page({ body: '<p>someco/app#12</p>' }) });
  assert.ok(checkDir(bad).some((p) => p.includes('someco/app#12')));
  const ok = fixture({ ...ALL, 'index.html': page({ body: '<p>acme/widgets#12 octo/x#1 rzorzal/clusia#3</p>' }) });
  assert.deepEqual(checkDir(ok), []);
});

test('a page without the install data-copy fails', () => {
  const dir = fixture({ ...ALL, 'index.html': page({ copy: '<code>curl nothing</code>' }) });
  assert.ok(checkDir(dir).some((p) => /data-copy/.test(p)));
});

test('a missing local file, a missing anchor and an external script fail', () => {
  const dir = fixture({
    'index.html': page({
      head: '<script src="https://cdn.example.com/a.js"></script>',
      body: '<a href="nope.html">n</a><a href="#ghost">g</a>',
    }),
  });
  const problems = checkDir(dir);
  assert.ok(problems.some((p) => p.includes('nope.html')));
  assert.ok(problems.some((p) => p.includes('#ghost')));
  assert.ok(problems.some((p) => p.includes('cdn.example.com')));
});

test('only fonts.googleapis.com is an allowed external link host', () => {
  const ok = fixture({ ...ALL, 'index.html': page({ head: '<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Inter">' }) });
  assert.deepEqual(checkDir(ok), []);
  const bad = fixture({ ...ALL, 'index.html': page({ head: '<link rel="stylesheet" href="https://evil.example/x.css">' }) });
  assert.ok(checkDir(bad).some((p) => p.includes('evil.example')));
});

test('Google Fonts links on both of its hosts pass, any other host still fails', () => {
  const head = '<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin><link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Inter">';
  assert.deepEqual(checkDir(fixture({ ...ALL, 'index.html': page({ head }) })), []);
  const bad = fixture({ ...ALL, 'index.html': page({ head: head + '<link rel="preconnect" href="https://other.example">' }) });
  assert.ok(checkDir(bad).some((p) => p.includes('other.example')));
});

test('a non-demo owner/repo#N in an alt, title or content attribute fails', () => {
  const img = fixture({ ...ALL, 'index.html': page({ body: '<img src="a.png" alt="someco/app#3" width="1" height="1">' }) });
  writeFileSync(join(img, 'a.png'), '');
  assert.ok(checkDir(img).some((p) => p.includes('someco/app#3')));
  const title = fixture({ ...ALL, 'index.html': page({ body: '<a href="#" title="someco/app#4">x</a>' }) });
  assert.ok(checkDir(title).some((p) => p.includes('someco/app#4')));
  const meta = fixture({ ...ALL, 'index.html': page({ head: '<meta name="description" content="someco/app#5">' }) });
  assert.ok(checkDir(meta).some((p) => p.includes('someco/app#5')));
});

test('a page without the nav links fails', () => {
  const dir = fixture({ 'index.html': `<!doctype html><html><body>${COPY}</body></html>` });
  assert.ok(checkDir(dir).some((p) => /nav/.test(p)));
});

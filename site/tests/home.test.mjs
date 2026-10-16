import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const SITE = fileURLToPath(new URL('..', import.meta.url));
const INSTALL = 'curl -fsSL https://raw.githubusercontent.com/rzorzal/clusia/main/install.sh | bash';
const INSTALL_BLOCK = `<div class="install"><code data-copy>${INSTALL}</code><button class="copy" aria-label="Copy install command">Copy</button></div>`;

const html = readFileSync(SITE + 'index.html', 'utf8');
const css = readFileSync(SITE + 'css/site.css', 'utf8');

const between = (text, open, close) => {
  const start = text.indexOf(open);
  if (start < 0) return '';
  const end = text.indexOf(close, start + open.length);
  return text.slice(start, end < 0 ? undefined : end + close.length);
};
const sectionWith = (heading) => {
  const at = html.indexOf(`>${heading}</h2>`);
  assert.ok(at > 0, heading);
  const start = html.lastIndexOf('<section', at);
  return html.slice(start, html.indexOf('</section>', at) + '</section>'.length);
};
const rule = (selector) => between(css, `\n${selector} {`, '}');

test('the home page is the story page', () => {
  assert.match(html, /<title>Clúsia — PR Reviews for Humans<\/title>/);
  assert.match(html, /<body class="home">/);
  assert.match(html, /<main class="story">/);
});

test('the hero sits on the sky with the lockup, the promise, the install block and two clear links', () => {
  const hero = between(html, '<section class="hero">', '</section>');
  assert.ok(hero);
  assert.match(hero, /<img class="on-light-sky" src="assets\/brand\/clusia-lockup\.svg" width="964" height="272" alt="Clúsia">/);
  assert.match(hero, /<img class="on-dark-sky" src="assets\/brand\/clusia-lockup-dark\.svg" width="964" height="272" alt="Clúsia">/);
  assert.match(hero, /<h1>PR Reviews for Humans<\/h1>/);
  assert.ok(hero.includes('A macOS app for reviewing pull requests carefully, with your own AI tools at your side. Free and open source.'));
  assert.ok(hero.includes(INSTALL_BLOCK), 'install block in the hero');
  assert.match(hero, /<a [^>]*href="install\.html"[^>]*>Install guide/);
  assert.match(hero, /<a [^>]*href="quick-start\.html"[^>]*>Quick start/);
  assert.ok(!html.includes('What it does'), 'the placeholder label is gone');
});

test('the story runs hero, three features, the way on, then the beach', () => {
  const order = [
    'class="hero"',
    '>A review you control</h2>',
    '>Your harness, your models</h2>',
    '>Always at hand</h2>',
    'href="features.html"',
    '>Install in one line</h2>',
    '<footer class="footer">',
    '<div class="sea" aria-hidden="true">',
  ];
  const main = between(html, '<main class="story">', '</main>');
  const at = order.map((s) => (s.startsWith('<footer') || s.startsWith('<div class="sea"') ? html.indexOf(s) : html.indexOf(s, html.indexOf('<main class="story">'))));
  at.forEach((i, n) => assert.ok(i > 0, order[n]));
  assert.deepEqual([...at].sort((a, b) => a - b), at);
  assert.ok(main.includes('>Install in one line</h2>'));
});

for (const [heading, shot] of [
  ['A review you control', 'sp1-m5b-review'],
  ['Your harness, your models', 'sp2-m1-agent-chat'],
  ['Always at hand', 'sp1-tray-readable'],
]) {
  test(`"${heading}" shows the ${shot} pair and reveals on scroll`, () => {
    const section = sectionWith(heading);
    assert.match(section, /^<section class="[^"]*\breveal\b/);
    assert.match(section, new RegExp(`<img class="shot-light" src="assets/${shot}-light\\.png"`));
    assert.match(section, new RegExp(`<img class="shot-dark" src="assets/${shot}-dark\\.png"`));
  });
}

test('the way on links every feature and the screens', () => {
  const section = sectionWith('Keep exploring');
  assert.match(section, /<a [^>]*href="features\.html"[^>]*>[\s\S]*See every feature/);
  assert.match(section, /<a [^>]*href="screens\.html"[^>]*>[\s\S]*Browse the screens/);
});

test('the beach ends the page with the install block', () => {
  const section = sectionWith('Install in one line');
  assert.ok(section.includes(INSTALL_BLOCK));
});

test('dunes and shrubs pass at their own speeds as the page scrolls', () => {
  const scenery = between(html, '<div class="scenery" aria-hidden="true">', '\n</div>');
  assert.ok(scenery, 'has home scenery');
  const speeds = [...scenery.matchAll(/data-drift="(-?[\d.]+)"/g)].map((m) => Number(m[1]));
  assert.ok(speeds.length >= 5, 'at least five drifting pieces');
  assert.ok(new Set(speeds).size >= 3, 'at three or more speeds');
  for (const s of speeds) assert.ok(Math.abs(s) > 0 && Math.abs(s) < 0.5, `speed ${s}`);
  for (const symbol of ['#clusia', '#tuft', '#bromeliad']) assert.ok(scenery.includes(`href="${symbol}"`), symbol);
});

test('the sea waits at the bottom: three wave bands whose shapes repeat with the keyframe periods', () => {
  const sea = between(html, '<div class="sea" aria-hidden="true">', '</div>');
  assert.ok(sea);
  assert.ok(html.indexOf(sea) > html.indexOf('</footer>'), 'the sea comes after the footer');
  for (const [n, period] of [[1, 200], [2, 240], [3, 280]]) {
    const band = between(sea, `<g class="wave wave-${n}">`, '</g>');
    assert.ok(band, `wave-${n}`);
    const d = band.match(/<path d="([^"]+)"/)[1];
    const xs = [...d.matchAll(/T(-?[\d.]+) /g)].map((m) => Number(m[1]));
    assert.ok(xs.length > 8, `wave-${n} has a long run of crests`);
    xs.slice(1).forEach((x, i) => assert.equal(x - xs[i], period / 2, `wave-${n} half period`));
    // The run has to outlast one period of travel on both sides of a 1600-wide view.
    assert.ok(Number(d.match(/^M(-?[\d.]+)/)[1]) <= -period && xs.at(-1) >= 1600 + period, `wave-${n} covers its travel`);
  }
  // The shared backdrop keeps its sea for the other pages; the home shows it only at the end.
  assert.match(rule('.home .restinga .wave'), /display: none;/);
});

test('the waves roll at 8, 11 and 14 seconds and stop under reduced motion', () => {
  assert.match(rule('.sea .wave-1'), /animation: swell 8s linear infinite;/);
  assert.match(rule('.sea .wave-2'), /animation: swell-2 11s linear infinite;/);
  assert.match(rule('.sea .wave-3'), /animation: swell-3 14s linear infinite;/);
  const still = between(css, '@media (prefers-reduced-motion: reduce) {', '\n}');
  assert.match(still, /\.sea \.wave[^{]*\{\s*animation: none;/);
});

test('hero text follows the brightness of the sky', () => {
  const light = rule('.home');
  assert.match(light, /--sky-ink: #1B1F1C;/);
  assert.match(between(css, '\n[data-sky="dark"] .home {', '}'), /--sky-ink: #FFF4E6;/);
  assert.match(between(css, '\n.home .hero h1', '}'), /color: var\(--sky-ink\);/);
  assert.match(css, /\[data-sky="dark"\] \.hero \.on-light-sky[^{]*\{\s*display: none;/);
  assert.match(css, /\.hero \.on-dark-sky[^{]*\{\s*display: none;/);
});

test('the footer does not repeat the install block right under the beach', () => {
  assert.match(css, /\.home \.footer-lead,\s*\.home \.footer \.install \{\s*display: none;/);
});

test('only one image of each screenshot pair shows', () => {
  // A selector heavier than .shot-dark that sets display would show both images at once.
  for (const m of css.matchAll(/\n([^\n{}]*\.shot[^\n{}]*)\{([^}]*)\}/g)) {
    const [selector, body] = [m[1].trim(), m[2]];
    if (!/display:/.test(body) || /shot-(light|dark)/.test(selector)) continue;
    assert.match(selector, /:where\(img\)/, selector);
  }
});

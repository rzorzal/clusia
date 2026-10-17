import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const SITE = fileURLToPath(new URL('..', import.meta.url));
const DOCS = fileURLToPath(new URL('../../docs/', import.meta.url));
const INSTALL = 'curl -fsSL https://raw.githubusercontent.com/rzorzal/clusia/main/install.sh | bash';
const INSTALL_BLOCK = `<div class="install"><code data-copy>${INSTALL}</code><button class="copy" aria-label="Copy install command">Copy</button></div>`;
const SHELL_PAGES = ['index.html', 'install.html', 'features.html', 'quick-start.html'];
// The home tells its story on the sky and the sand, so its main is not the floating card.
const MAIN_CLASS = { 'index.html': 'story', 'install.html': 'card', 'features.html': 'card card-wide', 'quick-start.html': 'card card-wide' };

const read = (name) => readFileSync(SITE + name, 'utf8');
const between = (html, open, close) => {
  const start = html.indexOf(open);
  if (start < 0) return '';
  const end = html.indexOf(close, start);
  return html.slice(start, end < 0 ? undefined : end + close.length);
};
const attr = (attrs, name) => (attrs.match(new RegExp(`\\s${name}="([^"]*)"`)) ?? [])[1] ?? null;

// Pixel size of the file behind an `assets/…` src: PNG from its IHDR chunk, SVG from its root width/height.
function intrinsicSize(src) {
  const file = src.startsWith('assets/') ? DOCS + src : SITE + src;
  const bytes = readFileSync(file);
  if (src.endsWith('.png')) return [bytes.readUInt32BE(16), bytes.readUInt32BE(20)];
  const root = bytes.toString('utf8').match(/<svg\b[^>]*>/)[0];
  return [Number(attr(root, 'width')), Number(attr(root, 'height'))];
}

for (const page of SHELL_PAGES) {
  test(`${page} uses the shared shell`, () => {
    const html = read(page);
    assert.match(html, /<html lang="en" data-scheme="light">/);
    assert.match(html, /<link rel="stylesheet" href="https:\/\/fonts\.googleapis\.com\/css2\?family=Inter[^"]*">/);
    assert.match(html, /<link rel="stylesheet" href="css\/site\.css">/);
    assert.match(html, /<script type="module" src="js\/site\.js"><\/script>/);

    const header = between(html, '<header class="nav">', '</header>');
    assert.ok(header, 'has <header class="nav">');
    assert.match(header, /<a class="brand" href="index\.html"[^>]*>/);
    assert.match(header, /<img class="lockup-light" src="assets\/brand\/clusia-lockup\.svg"/);
    assert.match(header, /<img class="lockup-dark" src="assets\/brand\/clusia-lockup-dark\.svg"/);
    for (const [href, label] of [['features.html', 'Features'], ['screens.html', 'Screens'], ['install.html', 'Install'], ['quick-start.html', 'Quick start']]) {
      assert.match(header, new RegExp(`<a href="${href}"[^>]*>${label}</a>`));
    }
    assert.match(header, /<a class="github" href="https:\/\/github\.com\/rzorzal\/clusia" aria-label="Clúsia on GitHub">\s*<svg/);

    const restinga = between(html, '<div class="restinga" aria-hidden="true">', '</div>');
    assert.ok(restinga, 'has the restinga backdrop');
    const depths = [...restinga.matchAll(/<g class="layer" data-depth="([\d.]+)"/g)].map((m) => m[1]);
    assert.deepEqual(depths, ['0.1', '0.25', '0.45', '0.6']);

    assert.match(html, new RegExp(`<main class="${MAIN_CLASS[page]}">[\\s\\S]+</main>`));
    const footer = between(html, '<footer', '</footer>');
    assert.ok(footer.includes(INSTALL_BLOCK), 'footer carries the install block');
    assert.match(footer, /href="https:\/\/github\.com\/rzorzal\/clusia"/);
    assert.match(footer, /<a href="https:\/\/github\.com\/rzorzal\/clusia\/blob\/main\/LICENSE">Apache License 2\.0<\/a>/);
    assert.match(footer, /href="https:\/\/github\.com\/users\/rzorzal\/projects\/1"/);
  });

  test(`${page} gives every image its intrinsic pixel size`, () => {
    const imgs = [...read(page).matchAll(/<img\b([^>]*)>/g)].map((m) => m[1]);
    assert.ok(imgs.length > 0);
    for (const a of imgs) {
      const src = attr(a, 'src');
      assert.ok(existsSync(src.startsWith('assets/') ? DOCS + src : SITE + src), `${src} exists`);
      assert.deepEqual([Number(attr(a, 'width')), Number(attr(a, 'height'))], intrinsicSize(src), src);
    }
  });
}

test('every page copies the restinga and the footer exactly', () => {
  const [first, ...rest] = SHELL_PAGES.map(read);
  for (const [open, close] of [['<div class="restinga" aria-hidden="true">', '</svg>\n</div>'], ['<footer class="footer">', '</footer>']]) {
    const want = between(first, open, close);
    assert.ok(want, open);
    rest.forEach((html, i) => assert.equal(between(html, open, close), want, `${SHELL_PAGES[i + 1]}: ${open}`));
  }
});

test('install.html walks through the README install facts', () => {
  const html = read('install.html');
  assert.match(html, /<title>Install Clúsia<\/title>/);
  assert.match(html, /<h1[^>]*>Install Clúsia<\/h1>/);
  const main = between(html, '<main class="card">', '</main>');
  assert.ok(main.includes(INSTALL_BLOCK), 'the page body carries the install block');
  for (const heading of ['What the script does', 'Requirements', 'Options', 'Homebrew by hand', 'Read the script first', 'From a clone', 'Updating', 'Uninstall']) {
    assert.match(main, new RegExp(`<h2[^>]*>${heading}</h2>`), heading);
  }
  for (const [flag, env] of [['--dry-run', 'CLUSIA_DRY_RUN'], ['--no-brew', 'CLUSIA_NO_BREW'], ['--no-open', 'CLUSIA_NO_OPEN'], ['--ref REF', 'CLUSIA_REF']]) {
    assert.match(main, new RegExp(`<td><code>${flag}</code></td>\\s*<td><code>${env}</code></td>`), flag);
  }
  for (const cmd of [
    `${INSTALL} -s -- --no-brew`,
    'brew install --HEAD rzorzal/clusia/clusia',
    'clusia install --from &quot;$(brew --prefix clusia)/libexec/bin&quot;',
    'curl -fsSL https://raw.githubusercontent.com/rzorzal/clusia/main/install.sh -o install.sh',
    'cargo run --release -p clusia -- install',
    'clusia uninstall',
  ]) {
    assert.ok(main.includes(cmd), cmd);
  }
  assert.match(main, /<a [^>]*href="quick-start\.html"[^>]*>/);
});

test('site.css pins the scheme colors, the card glass and the screenshot pair', () => {
  const css = read('css/site.css');
  const light = between(css, ':root {', '}');
  const dark = between(css, '[data-scheme="dark"] {', '}');
  for (const [name, value] of [['--card', '#ffffff'], ['--ink', '#1B1F1C'], ['--muted', '#5D665F'], ['--accent', '#2F9E44']]) {
    assert.ok(light.includes(`${name}: ${value};`), `light ${name}`);
  }
  for (const [name, value] of [['--card', '#23282a'], ['--ink', '#E9ECE9'], ['--muted', '#9AA49C'], ['--accent', '#4CC35F']]) {
    assert.ok(dark.includes(`${name}: ${value};`), `dark ${name}`);
  }
  assert.match(css, /color-mix\(in srgb, var\(--card\) 94%, transparent\)/);
  assert.match(css, /backdrop-filter: blur\(6px\)/);
  assert.match(css, /\[data-scheme="dark"\] \.shot-light[^{]*\{\s*display: none;/);
  assert.match(css, /\.shot-dark[^{]*\{\s*display: none;/);
  assert.match(css, /@media \(prefers-reduced-motion: reduce\)/);
  // Accent text is darker than the accent fill in light, for 4.5:1 on the white card.
  assert.ok(light.includes('--accent-text: #1F7A33;'));
  assert.ok(light.includes('--on-accent: #0c0e0d;'));
  assert.ok(dark.includes('--accent-text: #4CC35F;'));
  for (const sel of ['a {', '.eyebrow {', '.links a[aria-current="page"] {', '.next span:last-child {']) {
    const rule = between(css, `\n${sel}`, '}');
    assert.match(rule, /\bcolor: var\(--accent-text\);/, sel);
  }
  // From 600 px up the whole command shows, wrapped if it must; below it scrolls in its box.
  const wide = between(css, '@media (min-width: 600px) {', '\n}');
  assert.match(wide, /\.install code \{[^}]*white-space: normal;[^}]*overflow-wrap: anywhere;/);
  assert.match(between(css, '\n.install code {', '}'), /white-space: nowrap;/);
  // Without JavaScript the landscape still has its day colors.
  assert.ok(light.includes('--sky-top: #e9f5ec;'));
});

// The element a tag opens, up to its matching close tag, counting nested tags of the same name.
function elementAt(html, index, name) {
  const re = new RegExp(`<(/?)${name}\\b[^>]*>`, 'gi');
  re.lastIndex = index;
  let depth = 0;
  for (let m; (m = re.exec(html)); ) {
    depth += m[1] ? -1 : 1;
    if (depth === 0) return html.slice(index, m.index + m[0].length);
  }
  return html.slice(index);
}

for (const page of SHELL_PAGES) {
  test(`${page} never hides the install command behind a reveal`, () => {
    const html = read(page);
    for (const m of html.matchAll(/<([a-z][a-z0-9]*)\b[^>]*\bclass="[^"]*\breveal\b[^"]*"[^>]*>/gi)) {
      assert.ok(!/\sdata-copy\b/.test(elementAt(html, m.index, m[1])), `${m[0]} contains a data-copy element`);
    }
  });
}

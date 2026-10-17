import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, readFileSync, statSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const SITE = fileURLToPath(new URL('..', import.meta.url));
const DOCS = fileURLToPath(new URL('../../docs/', import.meta.url));
const BASE = 'https://rzorzal.github.io/clusia/';
const PAGES = ['index.html', 'features.html', 'screens.html', 'install.html', 'quick-start.html'];
const read = (name) => readFileSync(SITE + name, 'utf8');
const canonical = (page) => (page === 'index.html' ? BASE : BASE + page);
const meta = (html, key, value) =>
  [...html.matchAll(/<meta\b([^>]*)>/g)].map((m) => m[1]).find((a) => a.includes(`${key}="${value}"`))?.match(/content="([^"]*)"/)?.[1];
const pngSize = (file) => {
  const b = readFileSync(file);
  return [b.readUInt32BE(16), b.readUInt32BE(20)];
};

for (const page of [...PAGES, '404.html']) {
  test(`${page} has the share head`, () => {
    const html = read(page);
    const description = meta(html, 'name', 'description');
    assert.ok(description && description.length <= 160, `description of ${description?.length} chars`);
    const og = (p) => meta(html, 'property', p);
    assert.equal(og('og:type'), 'website');
    assert.equal(og('og:site_name'), 'Clúsia');
    assert.ok(og('og:title') && og('og:description'));
    assert.equal(og('og:image'), BASE + 'assets/site-og.png');
    assert.equal(og('og:image:width'), '1200');
    assert.equal(og('og:image:height'), '630');
    assert.ok(og('og:image:alt'));
    assert.equal(og('og:locale'), 'en_US');
    assert.equal(meta(html, 'name', 'twitter:card'), 'summary_large_image');
    for (const k of ['twitter:title', 'twitter:description', 'twitter:image', 'twitter:image:alt']) assert.ok(meta(html, 'name', k), k);
    assert.match(html, /<meta name="theme-color" content="#E9F5EC" media="\(prefers-color-scheme: light\)">/);
    assert.match(html, /<meta name="theme-color" content="#0c0e0d" media="\(prefers-color-scheme: dark\)">/);
    assert.match(html, /<link rel="icon" href="assets\/brand\/clusia-mark\.svg" type="image\/svg\+xml">/);
    assert.match(html, /<link rel="icon" type="image\/png" sizes="32x32" href="assets\/brand\/favicon-32\.png">/);
    assert.match(html, /<link rel="apple-touch-icon" href="assets\/brand\/apple-touch-icon\.png">/);
    assert.match(html, /<link rel="image_src" href="assets\/site-og\.png">/);
    if (page !== '404.html') {
      assert.ok(html.includes(`<link rel="canonical" href="${canonical(page)}">`));
      assert.equal(og('og:url'), canonical(page));
    }
  });
}

test('the descriptions are unique', () => {
  const all = PAGES.map((p) => meta(read(p), 'name', 'description'));
  assert.equal(new Set(all).size, PAGES.length);
});

test('the 404 stays on the beach: base, noindex, a way home and to the install page', () => {
  const html = read('404.html');
  assert.match(html, /<base href="https:\/\/rzorzal\.github\.io\/clusia\/">/);
  assert.match(html, /<meta name="robots" content="noindex">/);
  assert.match(html, /This page drifted out to sea/);
  assert.match(html, /<div class="restinga" aria-hidden="true">/);
  assert.match(html, /<a [^>]*href="index\.html"[^>]*>Home<\/a>/);
  assert.match(html, /<a [^>]*href="install\.html"[^>]*>Install<\/a>/);
  assert.ok(!html.includes('rel="canonical"'));
});

test('the home carries the SoftwareApplication JSON-LD and the sitemap link', () => {
  const html = read('index.html');
  const raw = html.match(/<script type="application\/ld\+json">([\s\S]*?)<\/script>/)?.[1];
  assert.ok(raw, 'has JSON-LD');
  const ld = JSON.parse(raw);
  assert.equal(ld['@type'], 'SoftwareApplication');
  assert.equal(ld.name, 'Clúsia');
  assert.equal(ld.url, BASE);
  assert.equal(ld.applicationCategory, 'DeveloperApplication');
  assert.equal(ld.operatingSystem, 'macOS');
  assert.deepEqual(ld.offers, { '@type': 'Offer', price: '0', priceCurrency: 'USD' });
  assert.equal(ld.license, 'https://www.apache.org/licenses/LICENSE-2.0');
  assert.equal(ld.image, BASE + 'assets/site-og.png');
  assert.equal(ld.installUrl, BASE + 'install.html');
  assert.equal(ld.sameAs, 'https://github.com/rzorzal/clusia');
  assert.ok(ld.description);
  assert.match(html, /<link rel="sitemap" type="application\/xml" href="sitemap\.xml">/);
});

test('the sitemap lists the five canonical pages and nothing else', () => {
  const xml = readFileSync(SITE + 'sitemap.xml', 'utf8');
  assert.deepEqual([...xml.matchAll(/<loc>([^<]*)<\/loc>/g)].map((m) => m[1]), PAGES.map(canonical));
  assert.ok(!xml.includes('lastmod'));
  assert.ok(!existsSync(SITE + 'robots.txt'));
});

test('the share card, its renderer and the icons are committed at the right size', () => {
  assert.ok(existsSync(SITE + 'tools/og-card.html'));
  assert.ok(existsSync(SITE + 'tools/og-card.sh'));
  assert.deepEqual(pngSize(DOCS + 'assets/site-og.png'), [1200, 630]);
  assert.ok(statSync(DOCS + 'assets/site-og.png').size < 400 * 1024);
  assert.deepEqual(pngSize(DOCS + 'assets/brand/favicon-32.png'), [32, 32]);
  assert.deepEqual(pngSize(DOCS + 'assets/brand/apple-touch-icon.png'), [180, 180]);
});

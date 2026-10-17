import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const SITE = fileURLToPath(new URL('..', import.meta.url));
const PAGES = ['index.html', 'features.html', 'screens.html', 'install.html', 'quick-start.html'];
const css = readFileSync(SITE + 'css/site.css', 'utf8');
// Every rule block whose selector list names `selector`.
const rules = (selector) =>
  [...css.matchAll(/(?:^|\n)([^{}\n][^{}]*)\{([^}]*)\}/g)]
    .filter(([, list]) => list.replace(/\/\*[\s\S]*?\*\//g, '').split(',').some((s) => s.trim() === selector))
    .map(([, , body]) => body);

test('the copy button is hidden when JavaScript is off', () => {
  assert.match(css, /html:not\(\.js\) \.copy\s*\{\s*display:\s*none;\s*\}/);
});

test('the large surfaces carry no backdrop blur', () => {
  for (const selector of ['.card', '.footer', '.panel']) {
    assert.ok(rules(selector).length > 0, selector);
    for (const body of rules(selector)) assert.doesNotMatch(body, /backdrop-filter/, selector);
  }
});

test('every horizontally scrollable container can be focused and has a label', () => {
  for (const name of PAGES) {
    const html = readFileSync(SITE + name, 'utf8');
    const tags = [
      ...html.matchAll(/<div class="install"><(code)\b([^>]*)>/g),
      ...html.matchAll(/<div class="code"><(pre)\b([^>]*)>/g),
      ...html.matchAll(/<(div) class="table-scroll[^"]*"([^>]*)>/g),
    ];
    for (const [, tag, attrs] of tags) {
      assert.match(attrs, /\stabindex="0"/, `${name}: <${tag}>`);
      assert.match(attrs, /\srole="region"/, `${name}: <${tag}>`);
      assert.match(attrs, /\saria-label="[^"]+"/, `${name}: <${tag}>`);
    }
    assert.ok(tags.length > 0, name);
  }
});

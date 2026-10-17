import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const INSTALL = 'curl -fsSL https://raw.githubusercontent.com/rzorzal/clusia/main/install.sh | bash';
const PAGES = ['index.html', 'features.html', 'screens.html', 'install.html', 'quick-start.html'];
const REPO_URL = 'https://github.com/rzorzal/clusia';
const DEMO_OWNERS = new Set(['rzorzal', 'acme', 'octo']);
const ALLOWED_LINK_HOSTS = new Set(['fonts.googleapis.com', 'fonts.gstatic.com']);

const htmlFiles = (dir) =>
  readdirSync(dir, { withFileTypes: true }).flatMap((e) => {
    const p = join(dir, e.name);
    if (e.isDirectory()) return htmlFiles(p);
    return e.name.endsWith('.html') ? [p] : [];
  });

// Attributes are read per tag so that `href` inside text or a data-* value is never mistaken for a link.
const tagsOf = (html, name) => [...html.matchAll(new RegExp(`<${name}\\b([^>]*)>`, 'gi'))].map((m) => m[1]);
const attr = (attrs, name) => {
  const m = attrs.match(new RegExp(`(?:^|\\s)${name}\\s*=\\s*(?:"([^"]*)"|'([^']*)'|([^\\s>]+))`, 'i'));
  return m ? (m[1] ?? m[2] ?? m[3]) : null;
};
const hasAttr = (attrs, name) => new RegExp(`(?:^|\\s)${name}(?=[\\s=/]|$)`, 'i').test(attrs);

const isExternal = (url) => /^(?:[a-z][a-z0-9+.-]*:|\/\/)/i.test(url);
const stripComments = (html) => html.replace(/<!--[\s\S]*?-->/g, '');
// Tags become `gap`: a space keeps neighbouring words apart, nothing keeps an element's own text exact.
const textOf = (html, gap = ' ') =>
  stripComments(html)
    .replace(/<(script|style)\b[\s\S]*?<\/\1>/gi, gap)
    .replace(/<[^>]*>/g, gap);

export function checkDir(dir) {
  const root = resolve(dir);
  const problems = [];
  const fail = (file, msg) => problems.push(`${relative(root, file)}: ${msg}`);

  for (const page of PAGES) {
    if (!existsSync(join(root, page))) problems.push(`${page}: the page is missing`);
  }

  for (const file of htmlFiles(root)) {
    const html = stripComments(readFileSync(file, 'utf8'));
    const ids = new Set([...html.matchAll(/\sid\s*=\s*(?:"([^"]*)"|'([^']*)')/gi)].map((m) => m[1] ?? m[2]));
    const refs = [
      ...tagsOf(html, '[a-z][a-z0-9]*').map((a) => ['href', attr(a, 'href')]),
      ...tagsOf(html, '[a-z][a-z0-9]*').map((a) => ['src', attr(a, 'src')]),
    ].filter(([, v]) => v !== null && v !== '');

    for (const [kind, url] of refs) {
      if (url.startsWith('/')) {
        fail(file, `${kind}="${url}" starts with "/"; use a relative path`);
        continue;
      }
      if (isExternal(url)) continue;
      if (url.startsWith('#')) {
        if (!ids.has(decodeURIComponent(url.slice(1)))) fail(file, `${kind}="${url}" has no matching id on the page`);
        continue;
      }
      const path = url.split(/[?#]/)[0];
      let target = resolve(dirname(file), decodeURIComponent(path));
      if (existsSync(target) && statSync(target).isDirectory()) target = join(target, 'index.html');
      if (existsSync(target)) continue;
      fail(file, `${kind}="${url}" does not resolve to a file`);
    }

    for (const a of tagsOf(html, 'img')) {
      const src = attr(a, 'src') ?? '(no src)';
      if (!(attr(a, 'alt') ?? '').trim()) fail(file, `<img ${src}> needs a non-empty alt`);
      for (const dim of ['width', 'height']) {
        if (!(attr(a, dim) ?? '').trim()) fail(file, `<img ${src}> needs a ${dim}`);
      }
    }

    const navs = [...html.matchAll(/<nav\b[^>]*>([\s\S]*?)<\/nav>/gi)].map((m) => m[1]);
    if (navs.length === 0) {
      fail(file, 'has no <nav>');
    } else {
      const hrefs = new Set(navs.flatMap((n) => tagsOf(n, 'a').map((a) => attr(a, 'href'))));
      for (const want of [...PAGES, REPO_URL]) {
        if (!hrefs.has(want)) fail(file, `<nav> does not link ${want}`);
      }
    }

    for (const a of tagsOf(html, 'script')) {
      const src = attr(a, 'src');
      if (src && isExternal(src)) fail(file, `external <script src="${src}"> is not allowed`);
    }
    for (const a of tagsOf(html, 'link')) {
      const href = attr(a, 'href');
      if (!href || !isExternal(href)) continue;
      let host = '';
      try {
        host = new URL(href, 'https://x.invalid').hostname;
      } catch {}
      if (!ALLOWED_LINK_HOSTS.has(host)) fail(file, `external <link href="${href}"> is not allowed`);
    }

    // Attribute text is visible too (alt, tooltips, link previews), so it is scanned along with the body.
    const attrText = [...html.matchAll(/(?:^|\s)(?:alt|title|content)\s*=\s*(?:"([^"]*)"|'([^']*)')/gi)].map((m) => m[1] ?? m[2]).join(' ');
    for (const m of `${textOf(html)} ${attrText}`.matchAll(/\b([A-Za-z0-9-]+)\/[A-Za-z0-9._-]+#\d+/g)) {
      if (!DEMO_OWNERS.has(m[1])) fail(file, `"${m[0]}" is not a demo repository (owner must be rzorzal, acme or octo)`);
    }

    // Scanned per opening tag: a global match on whole elements would swallow a data-copy element nested in a wrapper.
    const copies = [...html.matchAll(/<([a-z][a-z0-9]*)\b([^>]*)>/gi)]
      .filter(([, , attrs]) => hasAttr(attrs, 'data-copy'))
      .map((m) => {
        const rest = html.slice(m.index + m[0].length);
        return textOf(rest.slice(0, rest.search(new RegExp(`</${m[1]}>`, 'i'))), '');
      });
    if (!copies.includes(INSTALL)) {
      fail(file, 'needs an element with data-copy whose text is exactly the install command');
    }
  }
  return problems;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const dir = process.argv[2];
  if (!dir) {
    console.error('usage: node check.mjs OUT_DIR');
    process.exit(2);
  }
  const problems = checkDir(dir);
  for (const p of problems) console.log(p);
  process.exit(problems.length > 0 ? 1 : 0);
}

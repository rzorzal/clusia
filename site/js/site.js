import { paletteAt, schemeAt, momentAt, skyAt, hourFrom } from './palette.js';

export function paint(root, hour) {
  for (const [name, value] of Object.entries(paletteAt(hour))) root.style.setProperty(name, value);
  root.dataset.scheme = schemeAt(hour);
  root.dataset.moment = momentAt(hour);
  root.dataset.sky = skyAt(hour);
}

// Past this much scroll the layers stop moving, so on a long page the sea never
// climbs over the hills behind it.
export const PARALLAX_RANGE = 500;

// `|| 0` turns -0 into 0, so the transform never reads `-0px`.
export const layerShift = (scrollY, depth) => Math.min(Math.max(scrollY, 0), PARALLAX_RANGE) * -depth || 0;

// A drifting piece is offset against its distance from the middle of the screen, so it sits
// where the layout put it when centred and passes slower (positive speed) or faster (negative)
// than the page around it. The offset stays bounded however long the page is.
export const driftShift = (fromCentre, speed) => Math.round(-fromCentre * speed) || 0;

// Pieces more than a viewport above or below the screen keep their last shift: they cannot be seen.
export function driftPlan(pieces, scrollY, viewport) {
  const plan = [];
  pieces.forEach((p, i) => {
    const top = p.top - scrollY;
    if (top + p.height < -viewport || top > 2 * viewport) return;
    plan.push([i, driftShift(top + p.height / 2 - viewport / 2, p.speed)]);
  });
  return plan;
}

// `env` stands in for the window so the copy path can run outside a browser. The button's
// aria-label hides its changing text from screen readers, so `status` (a live region) says it.
export async function copyFrom(button, env = globalThis, status = null) {
  const code = button.parentElement.querySelector('[data-copy]');
  let copied = false;
  try {
    await env.navigator.clipboard.writeText(code.textContent);
    copied = true;
  } catch {
    // No clipboard API (plain http, older browsers) or permission denied: select the
    // text so ⌘C works, and try the legacy copy on the selection.
    const range = env.document.createRange();
    range.selectNodeContents(code);
    const selection = env.getSelection();
    selection.removeAllRanges();
    selection.addRange(range);
    try {
      copied = env.document.execCommand('copy');
    } catch {}
  }
  button.textContent = copied ? 'Copied' : 'Press ⌘C';
  if (status) status.textContent = copied ? 'Copied' : 'Selected. Press ⌘C to copy.';
  env.clearTimeout(button.resetTimer);
  button.resetTimer = env.setTimeout(() => {
    button.textContent = 'Copy';
  }, 1500);
}

function parallax(layers, pieces) {
  let queued = false;
  // Where the layout put each piece, in page coordinates, taken once and again on resize or
  // load (web fonts can move things); the shift applied so far is taken back out.
  const measure = () => {
    for (const p of pieces) {
      const box = p.el.getBoundingClientRect();
      p.top = box.top + scrollY - p.shift;
      p.height = box.height;
    }
  };
  const update = () => {
    queued = false;
    // No layout is read here, so every write below lands in one pass.
    const plan = driftPlan(pieces, scrollY, innerHeight);
    for (const [layer, depth] of layers) layer.style.transform = `translate3d(0, ${layerShift(scrollY, depth)}px, 0)`;
    for (const [i, shift] of plan) {
      const p = pieces[i];
      if (shift === p.shift) continue;
      p.shift = shift;
      p.el.style.transform = `translate3d(0, ${shift}px, 0)`;
    }
  };
  const queue = () => {
    if (!queued) {
      queued = true;
      requestAnimationFrame(update);
    }
  };
  const remeasure = () => {
    measure();
    queue();
  };
  addEventListener('scroll', queue, { passive: true });
  addEventListener('resize', remeasure);
  addEventListener('load', remeasure);
  measure();
  update();
}

function reveal(items) {
  // Whatever is on screen at load is shown as is; only what scrolls in later animates.
  const observer = new IntersectionObserver((entries) => {
    for (const entry of entries) {
      if (!entry.isIntersecting) continue;
      entry.target.classList.add('is-visible');
      observer.unobserve(entry.target);
    }
  }, { rootMargin: '0px 0px -8% 0px' });
  for (const item of items) {
    if (item.getBoundingClientRect().top < innerHeight) item.classList.add('is-visible');
    else observer.observe(item);
  }
}

function init() {
  const root = document.documentElement;
  paint(root, hourFrom(new Date(), location.search));

  const still = matchMedia('(prefers-reduced-motion: reduce)').matches;
  const items = document.querySelectorAll('.reveal');
  if (still || !('IntersectionObserver' in window)) items.forEach((item) => item.classList.add('is-visible'));
  else reveal(items);
  root.classList.add('js');

  if (!still) {
    parallax(
      [...document.querySelectorAll('.restinga .layer')].map((layer) => [layer, Number(layer.dataset.depth) || 0]),
      [...document.querySelectorAll('[data-drift]')].map((el) => ({ el, speed: Number(el.dataset.drift) || 0, shift: 0, top: 0, height: 0 })),
    );
  }

  const status = document.createElement('p');
  status.className = 'visually-hidden';
  status.setAttribute('role', 'status');
  status.setAttribute('aria-live', 'polite');
  document.body.append(status);
  for (const button of document.querySelectorAll('button.copy')) {
    button.addEventListener('click', () => {
      // Emptied first so the same message twice in a row is announced again.
      status.textContent = '';
      copyFrom(button, globalThis, status);
    });
  }
}

if (typeof document !== 'undefined') init();

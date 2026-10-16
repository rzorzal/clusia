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

function parallax(layers, drifters) {
  let queued = false;
  const update = () => {
    queued = false;
    for (const [layer, depth] of layers) layer.style.transform = `translate3d(0, ${layerShift(scrollY, depth)}px, 0)`;
    // All reads before any write. The box measured includes the shift applied last time,
    // which is taken back out to find where the layout put the piece.
    const middle = innerHeight / 2;
    const shifts = drifters.map((d) => {
      const box = d.el.getBoundingClientRect();
      return driftShift(box.top + box.height / 2 - d.shift - middle, d.speed);
    });
    drifters.forEach((d, i) => {
      if (shifts[i] === d.shift) return;
      d.shift = shifts[i];
      d.el.style.transform = `translate3d(0, ${d.shift}px, 0)`;
    });
  };
  const queue = () => {
    if (!queued) {
      queued = true;
      requestAnimationFrame(update);
    }
  };
  addEventListener('scroll', queue, { passive: true });
  addEventListener('resize', queue);
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
      [...document.querySelectorAll('[data-drift]')].map((el) => ({ el, speed: Number(el.dataset.drift) || 0, shift: 0 })),
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

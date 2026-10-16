// Colors and alphas for the illustrated landscape, blended between keyframes
// placed at hours of the day. Card, ink and button colors are not blended:
// they follow the light/dark scheme in CSS.

const NIGHT = {
  colors: {
    '--sky-top': '#0c0e0d', '--sky-bottom': '#1b2a33', '--sun': '#f07a4a',
    '--hill-back': '#18211c', '--hill-front': '#121a15', '--shrub': '#0a110c',
    '--sand-back': '#2a2620', '--sand-front': '#332e26',
    '--wave-1': '#1d3a46', '--wave-2': '#152c36', '--wave-3': '#0e1f27',
  },
  alpha: { '--sun-opacity': 0, '--moon-opacity': 1, '--stars-opacity': 1 },
};

const DAY = {
  colors: {
    '--sky-top': '#e9f5ec', '--sky-bottom': '#fff4e6', '--sun': '#f07a4a',
    '--hill-back': '#9ccfa4', '--hill-front': '#4cc35f', '--shrub': '#2f9e44',
    '--sand-back': '#ebd9b4', '--sand-front': '#f3e6c8',
    '--wave-1': '#7fc8d8', '--wave-2': '#3fa7c0', '--wave-3': '#2a7f9e',
  },
  alpha: { '--sun-opacity': 0.85, '--moon-opacity': 0, '--stars-opacity': 0 },
};

export const KEYFRAMES = [
  { hour: 0, ...NIGHT },
  { hour: 5, ...NIGHT },
  {
    hour: 6.5,
    colors: {
      '--sky-top': '#7d8fb3', '--sky-bottom': '#f6c7b6', '--sun': '#f7a07a',
      '--hill-back': '#7fae8a', '--hill-front': '#3f9a50', '--shrub': '#2a7a3a',
      '--sand-back': '#d9c4a0', '--sand-front': '#e8d6b6',
      '--wave-1': '#8fb7c9', '--wave-2': '#5f98b0', '--wave-3': '#3d7590',
    },
    alpha: { '--sun-opacity': 0.9, '--moon-opacity': 0, '--stars-opacity': 0 },
  },
  { hour: 9, ...DAY },
  { hour: 16, ...DAY },
  {
    hour: 18,
    colors: {
      '--sky-top': '#4a4f6e', '--sky-bottom': '#f07a4a', '--sun': '#d9572b',
      '--hill-back': '#5f7a55', '--hill-front': '#3c6a3c', '--shrub': '#24482a',
      '--sand-back': '#c9925f', '--sand-front': '#d9a676',
      '--wave-1': '#c46a50', '--wave-2': '#7a4f5a', '--wave-3': '#3d3a52',
    },
    alpha: { '--sun-opacity': 1, '--moon-opacity': 0, '--stars-opacity': 0 },
  },
  {
    hour: 20,
    colors: {
      '--sky-top': '#151a22', '--sky-bottom': '#6b3a3a', '--sun': '#d9572b',
      '--hill-back': '#26322a', '--hill-front': '#1c281f', '--shrub': '#111a13',
      '--sand-back': '#3a2a20', '--sand-front': '#4a3628',
      '--wave-1': '#2a3a48', '--wave-2': '#1d2c38', '--wave-3': '#122029',
    },
    alpha: { '--sun-opacity': 0.3, '--moon-opacity': 0.5, '--stars-opacity': 0.5 },
  },
  { hour: 21, ...NIGHT },
  { hour: 24, ...NIGHT },
];

const normalize = (hour) => ((hour % 24) + 24) % 24;

const channels = (hex) => [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16));

const mixColor = (a, b, t) => {
  const from = channels(a);
  const to = channels(b);
  return '#' + from
    .map((v, i) => Math.round(v + (to[i] - v) * t).toString(16).padStart(2, '0'))
    .join('');
};

// Three decimals keep the CSS short; String() turns 1 into '1' and 0.5 into '0.5'.
const formatNumber = (n) => String(Math.round(n * 1000) / 1000);

// Linear pieces through (hour, value) points; flat outside the first/last point.
const piecewise = (points, h) => {
  if (h <= points[0][0]) return points[0][1];
  for (let i = 1; i < points.length; i++) {
    const [h1, v1] = points[i];
    if (h <= h1) {
      const [h0, v0] = points[i - 1];
      return v0 + ((v1 - v0) * (h - h0)) / (h1 - h0);
    }
  }
  return points[points.length - 1][1];
};

// 0 is the top of the sky, 1 the horizon (the sun is out of sight there).
const SUN_Y = [[0, 1], [5, 1], [9, 0.15], [16, 0.15], [19, 1], [24, 1]];

export function paletteAt(hour) {
  const h = normalize(hour);
  let i = 0;
  while (i < KEYFRAMES.length - 2 && KEYFRAMES[i + 1].hour <= h) i++;
  const a = KEYFRAMES[i];
  const b = KEYFRAMES[i + 1];
  const t = (h - a.hour) / (b.hour - a.hour);

  const out = {};
  for (const name of Object.keys(a.colors)) out[name] = mixColor(a.colors[name], b.colors[name], t);
  for (const name of Object.keys(a.alpha)) {
    out[name] = formatNumber(a.alpha[name] + (b.alpha[name] - a.alpha[name]) * t);
  }
  out['--sun-y'] = formatNumber(piecewise(SUN_Y, h));
  return out;
}

export function momentAt(hour) {
  const h = normalize(hour);
  if (h >= 21 || h < 5) return 'night';
  if (h < 7) return 'dawn';
  if (h < 16) return 'day';
  if (h < 19) return 'sunset';
  return 'dusk';
}

export function schemeAt(hour) {
  const h = normalize(hour);
  return h >= 19.5 || h < 5.5 ? 'dark' : 'light';
}

// `?hour=H` previews any hour; anything that is not a number in [0, 24) is ignored.
export function hourFrom(date, search) {
  const raw = new URLSearchParams(search).get('hour');
  if (raw !== null && raw.trim() !== '') {
    const h = Number(raw);
    if (Number.isFinite(h) && h >= 0 && h < 24) return h;
  }
  return date.getHours() + date.getMinutes() / 60;
}

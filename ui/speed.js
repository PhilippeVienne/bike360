// Accélérés d'un clip : points de vitesse (×1 à ×16) posés dans l'éditeur, mêmes formules
// que core/src/ramp.rs (variation douce en échelle log entre deux points, constante avant le
// premier et après le dernier). Pas de ralenti : les sources sont à 29,97 i/s.
// Partage : SPEEDS, speedKeys, speedAt, outputDuration.

export const SPEEDS = [1, 2, 4, 8, 16];
const smooth = (u) => u * u * (3 - 2 * u);

/** Points de vitesse du clip, triés et bornés. */
export function speedKeys(c) {
  return (c.speed_keys || [])
    .filter((k) => Number.isFinite(k.t) && Number.isFinite(k.speed))
    .map((k) => ({ t: k.t, speed: Math.min(16, Math.max(1, k.speed)) }))
    .sort((a, b) => a.t - b.t);
}

/** Vitesse à tr secondes depuis le début du clip (1 sans point). */
export function speedAt(keys, tr) {
  if (!keys.length) return 1;
  if (tr <= keys[0].t) return keys[0].speed;
  const last = keys[keys.length - 1];
  if (tr >= last.t) return last.speed;
  const i = keys.findIndex((k) => k.t > tr), a = keys[i - 1], b = keys[i];
  const u = smooth((tr - a.t) / (b.t - a.t));
  return Math.exp(Math.log(a.speed) + (Math.log(b.speed) - Math.log(a.speed)) * u);
}

/** Durée du clip une fois accéléré (s), intégrée par pas de 1/30 s. */
export function outputDuration(c) {
  const keys = speedKeys(c), len = c.end - c.start;
  if (!keys.length) return len;
  let out = 0;
  for (let t = 0; t < len; t += 1 / 30) out += 1 / 30 / speedAt(keys, Math.min(len, t + 1 / 60));
  return out;
}

"""Résumé hyperlapse d'une session : toute la balade en quelques minutes, vitesse variable.

Le temps de sortie est réparti selon le score d'intérêt (analyze) : les moments forts sont
lents (quelques ×), les routes calmes très accélérées, les arrêts quasiment sautés. La
densité est lissée (pas d'à-coups) puis normalisée pour tenir la durée cible.
"""
import numpy as np

OUT_FPS = 30000 / 1001
MAX_DENSITY = 0.25       # au plus lent : 4× (seconde de sortie par seconde de source)
MIN_DENSITY = 1 / 400    # arrêts : ~400×
SMOOTH_S = 6.0           # lissage de la vitesse de lecture


def _gauss(x, sig):
    k = np.arange(-int(4 * sig), int(4 * sig) + 1)
    w = np.exp(-k ** 2 / (2 * sig ** 2))
    return np.convolve(np.pad(x, len(k) // 2, mode="edge"), w / w.sum(), "valid")


def _series(result, key, default):
    v = np.array([np.nan if x is None else x for x in result["series"][key]], float)
    return np.where(np.isnan(v), default, v)


def density(result, target_s):
    """Secondes de sortie par seconde de source, pour chaque seconde de la session."""
    score = np.clip(_series(result, "score", 0.0), 0, 1)
    speed = _series(result, "speed", 30.0)
    raw = 0.01 + 0.99 * score ** 2
    raw[speed < 5] = MIN_DENSITY
    raw = np.exp(_gauss(np.log(raw), SMOOTH_S))          # lissage en échelle log (facteurs)
    d = raw * target_s / raw.sum()
    for _ in range(20):                                 # bornes puis renormalisation
        d = np.clip(d, MIN_DENSITY, MAX_DENSITY)
        free = (d > MIN_DENSITY) & (d < MAX_DENSITY)
        excess = target_s - d.sum()
        if abs(excess) < 0.01 or not free.any():
            break
        d[free] *= 1 + excess / d[free].sum()
    return d


def frame_times(result, target_s):
    """Instants de session (s) des images de sortie, à OUT_FPS."""
    d = density(result, target_s)
    cum = np.concatenate([[0.0], np.cumsum(d)])          # temps de sortie à chaque seconde de source
    n_out = int(cum[-1] * OUT_FPS)
    out_t = np.arange(n_out) / OUT_FPS
    return np.interp(out_t, cum, np.arange(len(cum)))


def summary(result, target_s):
    d = density(result, target_s)
    return {"output_s": round(float(d.sum()), 1), "fastest_x": round(float(1 / d.min())),
            "slowest_x": round(float(1 / d.max()), 1)}

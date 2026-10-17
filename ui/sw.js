// Service worker de Bike360 (PWA) : utile surtout en bord de route avec un réseau instable
// (vérifier un trajet ou une station sans connexion ne marche pas, mais rouvrir l'appli oui).
//
// Portée de la racine ("/") exprès : les pages de montage (/ui/*.html) et les API (/api/*)
// restent toujours en direct, sans cache — seule la coquille (icônes, page hors-ligne) est
// mise en cache pour que l'appli s'ouvre même sans réseau.

const CACHE_VERSION = 'v1';
const STATIC_CACHE = `bike360-static-${CACHE_VERSION}`;
const OFFLINE_URL = '/offline.html';

const PRECACHE_URLS = [
  OFFLINE_URL,
  '/ui/icons/icon-192.png',
  '/ui/icons/icon-512.png',
];

self.addEventListener('install', (event) => {
  event.waitUntil(
    caches.open(STATIC_CACHE)
      .then((cache) => cache.addAll(PRECACHE_URLS))
      .catch(() => {})
  );
  self.skipWaiting();
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== STATIC_CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim())
  );
});

self.addEventListener('fetch', (event) => {
  const { request } = event;
  if (request.method !== 'GET') return;
  const url = new URL(request.url);

  // Icônes et manifeste : contenu stable, cache d'abord.
  if (url.pathname.startsWith('/ui/icons/') || url.pathname === '/manifest.json') {
    event.respondWith(
      caches.match(request).then((cached) => cached || fetch(request).then((resp) => {
        const clone = resp.clone();
        caches.open(STATIC_CACHE).then((cache) => cache.put(request, clone));
        return resp;
      }))
    );
    return;
  }

  // Navigation (ouverture d'une page) : réseau d'abord, page hors-ligne si injoignable.
  if (request.mode === 'navigate') {
    event.respondWith(
      fetch(request).catch(() => caches.match(OFFLINE_URL).then((cached) => cached || Response.error()))
    );
    return;
  }

  // Le reste (API, vidéo, musique…) : jamais de cache, les données doivent rester à jour.
  event.respondWith(fetch(request));
});

self.addEventListener('message', (event) => {
  if (event.data && event.data.type === 'SKIP_WAITING') self.skipWaiting();
});

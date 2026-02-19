// Supervillain PWA Service Worker
// NOTE: iOS Safari evicts service workers after ~7 days of non-use.
// The app must work without the service worker; it's for convenience only.

const CACHE_NAME = 'supervillain-v1';
const APP_SHELL = [
    '/mobile/',
    '/mobile/manifest.json',
];

self.addEventListener('install', (event) => {
    event.waitUntil(
        caches.open(CACHE_NAME)
            .then((cache) => cache.addAll(APP_SHELL))
            .then(() => self.skipWaiting())
    );
});

self.addEventListener('activate', (event) => {
    event.waitUntil(
        caches.keys()
            .then((keys) => Promise.all(
                keys.filter((k) => k !== CACHE_NAME).map((k) => caches.delete(k))
            ))
            .then(() => self.clients.claim())
    );
});

self.addEventListener('fetch', (event) => {
    const url = new URL(event.request.url);

    // Never cache JMAP API calls
    if (url.hostname === 'api.fastmail.com') return;

    // Network-first for app shell, fall back to cache
    event.respondWith(
        fetch(event.request)
            .then((resp) => {
                if (resp.ok) {
                    const clone = resp.clone();
                    caches.open(CACHE_NAME).then((cache) => cache.put(event.request, clone));
                }
                return resp;
            })
            .catch(() => caches.match(event.request))
    );
});

// Supervillain — shared server API client (desktop + mobile).
//
// Classic script, not an ES module: desktop app.js is a classic script and
// mobile app.js is a module. Top-level declarations here become globals for
// both (classic scripts finish executing before deferred module scripts run).

// Auto-append ?account= ONLY for account-scoped routes. Settings routes
// (`/accounts/...`, `/theme`, `/timezone*`) are global and must never be
// tagged.
const ACCOUNT_SCOPED_API = /^\/(emails|mailboxes|identities|splits|upload|split-counts|calendar|drafts)/;

// Error taxonomy: ApiAuthError means the account's provider session needs
// re-authorization (401/403 from the server); everything else — network
// failures and non-auth HTTP errors — is ApiError. Callers that redirect or
// banner on auth problems must test `instanceof ApiAuthError` BEFORE
// `instanceof ApiError` (the former extends the latter).
class ApiError extends Error {
    constructor(message, status = null) {
        super(message);
        this.name = 'ApiError';
        this.status = status;
    }
}

class ApiAuthError extends ApiError {
    constructor(message, status = null) {
        super(message, status);
        this.name = 'ApiAuthError';
    }
}

// makeApi(accountId) → async api(method, path, body, signal) bound to one
// account. Pass a falsy accountId for an unscoped instance (global routes,
// or before accounts are loaded). Make a new instance on account switch.
// The returned function also carries api.withMeta(...), which resolves to
// { data, headers } for the callers that need a response header (currently
// just loadEmails' stale-snapshot detection).
function makeApi(accountId) {
    async function request(method, path, body = null, signal = null) {
        const opts = {
            method,
            headers: { 'Content-Type': 'application/json' },
        };
        if (body) opts.body = JSON.stringify(body);
        if (signal) opts.signal = signal;

        let url = '/api' + path;
        if (accountId && ACCOUNT_SCOPED_API.test(path)) {
            const separator = url.includes('?') ? '&' : '?';
            url += `${separator}account=${encodeURIComponent(accountId)}`;
        }

        let resp;
        try {
            resp = await fetch(url, opts);
        } catch (err) {
            if (err.name === 'AbortError') throw err;
            throw new ApiError('Network error: ' + err.message);
        }
        if (resp.status === 401 || resp.status === 403) {
            throw new ApiAuthError(await resp.text(), resp.status);
        }
        if (!resp.ok) {
            throw new ApiError(await resp.text(), resp.status);
        }
        if (resp.status === 204) return { data: null, headers: resp.headers };
        const text = await resp.text();
        return { data: text ? JSON.parse(text) : null, headers: resp.headers };
    }

    async function api(method, path, body = null, signal = null) {
        return (await request(method, path, body, signal)).data;
    }
    api.withMeta = request;
    return api;
}

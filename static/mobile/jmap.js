// Supervillain — JMAP client module
// Ported from src/jmap.rs (session, connect, jmap_call)
//
// Token storage: localStorage is the only viable option for a PWA that talks
// directly to Fastmail's JMAP API (no backend proxy). The token is scoped to
// this origin only. Mitigations: host on a dedicated domain, personal device,
// locked phone. See THE-133 risk table.

const STORAGE_KEY = 'supervillain_session';
const SESSION_URL = 'https://api.fastmail.com/jmap/session';
const JMAP_USING = [
    'urn:ietf:params:jmap:core',
    'urn:ietf:params:jmap:mail',
    'urn:ietf:params:jmap:submission',
];

// ============================================================================
// Error types
// ============================================================================

export class JmapAuthError extends Error {
    constructor(message) {
        super(message);
        this.name = 'JmapAuthError';
    }
}

export class JmapNetworkError extends Error {
    constructor(message) {
        super(message);
        this.name = 'JmapNetworkError';
    }
}

// ============================================================================
// Session persistence
// ============================================================================

export function getSession() {
    try {
        const raw = localStorage.getItem(STORAGE_KEY);
        return raw ? JSON.parse(raw) : null;
    } catch {
        return null;
    }
}

export function saveSession(session) {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(session));
}

export function clearSession() {
    localStorage.removeItem(STORAGE_KEY);
}

// ============================================================================
// Session discovery
// ============================================================================

/**
 * Connect to Fastmail JMAP and return a session object.
 * Mirrors src/jmap.rs connect() (lines 47-81).
 *
 * @param {string} username - Fastmail username
 * @param {string} token - API token (Bearer)
 * @returns {Promise<Object>} Session with apiUrl, accountId, etc.
 * @throws {JmapAuthError} on 401/403
 * @throws {JmapNetworkError} on network failure or unexpected status
 */
export async function connect(username, token) {
    let resp;
    try {
        resp = await fetch(SESSION_URL, {
            headers: { 'Authorization': 'Bearer ' + token },
        });
    } catch (err) {
        throw new JmapNetworkError('Network error: ' + err.message);
    }

    if (resp.status === 401 || resp.status === 403) {
        throw new JmapAuthError('Invalid token');
    }
    if (!resp.ok) {
        throw new JmapNetworkError('Connection failed (' + resp.status + ')');
    }

    let data;
    try {
        data = await resp.json();
    } catch (err) {
        throw new JmapNetworkError('Invalid JSON in session response');
    }

    return {
        username,
        token,
        apiUrl: data.apiUrl,
        accountId: data.primaryAccounts['urn:ietf:params:jmap:mail'],
        uploadUrl: data.uploadUrl || null,
        downloadUrl: data.downloadUrl || null,
    };
}

// ============================================================================
// JMAP API call
// ============================================================================

/**
 * Make a JMAP method call. Mirrors src/jmap.rs jmap_call() (lines 83-115).
 *
 * @param {Object} session - Session object from connect() or getSession()
 * @param {Array} methodCalls - Array of JMAP method invocations
 *   e.g. [["Mailbox/get", { accountId: "..." }, "0"]]
 * @returns {Promise<Object>} Full JMAP response body
 * @throws {JmapAuthError} on 401/403 (token expired/revoked)
 * @throws {JmapNetworkError} on network failure or unexpected status
 */
export async function jmapCall(session, methodCalls) {
    if (!session?.apiUrl) {
        throw new JmapNetworkError('Not connected');
    }

    const payload = {
        using: JMAP_USING,
        methodCalls,
    };

    let resp;
    try {
        resp = await fetch(session.apiUrl, {
            method: 'POST',
            headers: {
                'Authorization': 'Bearer ' + session.token,
                'Content-Type': 'application/json',
            },
            body: JSON.stringify(payload),
        });
    } catch (err) {
        throw new JmapNetworkError('Network error: ' + err.message);
    }

    if (resp.status === 401 || resp.status === 403) {
        throw new JmapAuthError('Session expired');
    }
    if (!resp.ok) {
        throw new JmapNetworkError('JMAP call failed: HTTP ' + resp.status);
    }

    try {
        return await resp.json();
    } catch (err) {
        throw new JmapNetworkError('Invalid JSON in JMAP response');
    }
}

// ============================================================================
// Blob download URL
// ============================================================================

/**
 * Build a blob download URL from the session's downloadUrl template.
 * Mirrors src/jmap.rs download logic (lines 1007-1012).
 * Assumes Fastmail's simple RFC 6570 URI template form ({var}).
 *
 * @param {Object} session - Session object
 * @param {string} blobId - Blob ID
 * @param {string} name - Filename
 * @param {string} [type] - MIME type
 * @returns {string} Resolved download URL
 */
export function blobUrl(session, blobId, name, type) {
    if (!session?.downloadUrl) return null;
    return session.downloadUrl
        .replace('{accountId}', encodeURIComponent(session.accountId))
        .replace('{blobId}', encodeURIComponent(blobId))
        .replace('{name}', encodeURIComponent(name))
        .replace('{type}', encodeURIComponent(type || 'application/octet-stream'));
}

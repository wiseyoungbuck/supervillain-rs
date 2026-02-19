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

// ============================================================================
// Mailboxes & Identities
// ============================================================================

/**
 * Fetch all mailboxes. Mirrors src/jmap.rs get_mailboxes() (lines 117-147).
 */
export async function getMailboxes(session) {
    const resp = await jmapCall(session, [
        ['Mailbox/get', { accountId: session.accountId }, '0'],
    ]);
    const list = resp.methodResponses?.[0]?.[1]?.list;
    if (!list) throw new JmapNetworkError('Invalid Mailbox/get response');
    return list.map(m => ({
        id: m.id,
        name: m.name || '',
        role: m.role || null,
        totalEmails: m.totalEmails || 0,
        unreadEmails: m.unreadEmails || 0,
        parentId: m.parentId || null,
    }));
}

/**
 * Fetch identities. Mirrors src/jmap.rs get_identities() (lines 149-186).
 */
export async function getIdentities(session) {
    const resp = await jmapCall(session, [
        ['Identity/get', { accountId: session.accountId }, '0'],
    ]);
    const list = resp.methodResponses?.[0]?.[1]?.list;
    if (!list) throw new JmapNetworkError('Invalid Identity/get response');
    return list.map(i => ({
        id: i.id,
        email: i.email || '',
        name: i.name || '',
    }));
}

// ============================================================================
// Email query & fetch
// ============================================================================

/** List properties requested for email list view (no body). */
const LIST_PROPERTIES = [
    'id', 'blobId', 'threadId', 'mailboxIds', 'keywords',
    'receivedAt', 'subject', 'from', 'to', 'cc',
    'preview', 'hasAttachment', 'size',
];

/**
 * Query email IDs. Mirrors src/jmap.rs query_emails() (lines 200-235).
 */
export async function queryEmails(session, mailboxId, limit = 100, position = 0) {
    const filter = mailboxId ? { inMailbox: mailboxId } : {};
    const resp = await jmapCall(session, [
        ['Email/query', {
            accountId: session.accountId,
            filter,
            sort: [{ property: 'receivedAt', isAscending: false }],
            limit,
            position,
        }, '0'],
    ]);
    const ids = resp.methodResponses?.[0]?.[1]?.ids;
    if (!ids) throw new JmapNetworkError('Invalid Email/query response');
    return ids;
}

/**
 * Fetch emails by ID. Mirrors src/jmap.rs get_emails() (lines 237-302).
 * @param {boolean} fetchBody - true for detail view, false for list view
 */
export async function getEmails(session, ids, fetchBody = false) {
    if (!ids.length) return [];
    const properties = fetchBody
        ? [...LIST_PROPERTIES, 'textBody', 'htmlBody', 'bodyValues', 'bodyStructure']
        : LIST_PROPERTIES;
    const args = {
        accountId: session.accountId,
        ids,
        properties,
        fetchHTMLBodyValues: fetchBody,
        fetchTextBodyValues: fetchBody,
        maxBodyValueBytes: 1_000_000,
    };
    if (fetchBody) {
        args.bodyProperties = [
            'partId', 'blobId', 'type', 'name', 'size', 'disposition', 'subParts',
        ];
    }
    const resp = await jmapCall(session, [['Email/get', args, '0']]);
    const list = resp.methodResponses?.[0]?.[1]?.list;
    if (!list) throw new JmapNetworkError('Invalid Email/get response');
    return list.map(item => parseEmail(item, fetchBody));
}

/**
 * Parse a JMAP email response into a plain object.
 * Mirrors src/jmap.rs parse_jmap_email() (lines 304-394).
 */
function parseEmail(item, fetchBody) {
    const keywords = item.keywords || {};
    const from = (item.from || []).map(a => ({
        name: a.name || null,
        email: a.email || '',
    }));
    const to = (item.to || []).map(a => ({
        name: a.name || null,
        email: a.email || '',
    }));
    const cc = (item.cc || []).map(a => ({
        name: a.name || null,
        email: a.email || '',
    }));

    const email = {
        id: item.id,
        blobId: item.blobId,
        threadId: item.threadId,
        mailboxIds: item.mailboxIds || {},
        keywords,
        receivedAt: item.receivedAt,
        subject: item.subject || '',
        from,
        to,
        cc,
        preview: item.preview || '',
        hasAttachment: item.hasAttachment || false,
        size: item.size || 0,
        isUnread: !keywords['$seen'],
        isFlagged: !!keywords['$flagged'],
    };

    if (fetchBody) {
        const bv = item.bodyValues || {};
        const textParts = (item.textBody || [])
            .map(p => bv[p.partId]?.value).filter(Boolean);
        const htmlParts = (item.htmlBody || [])
            .map(p => bv[p.partId]?.value).filter(Boolean);
        email.textBody = textParts.length ? textParts.join('\n') : null;
        email.htmlBody = htmlParts.length ? htmlParts.join('\n') : null;
    }

    return email;
}

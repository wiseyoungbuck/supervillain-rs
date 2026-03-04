// Supervillain — Mobile app logic
// Email list view with pull-to-refresh and infinite scroll.

import {
    connect, getSession, saveSession, clearSession,
    getMailboxes, getIdentities, queryEmails, getEmails,
    findAttachments, findCalendarBlobId, markRead, blobUrl,
    JmapAuthError,
} from '/mobile/jmap.js';

// ============================================================================
// State
// ============================================================================

const state = {
    session: null,
    mailboxes: [],
    identities: [],
    inboxId: null,
    emails: [],
    loading: false,
    loadingMore: false,
    currentView: 'list',      // 'list' | 'detail'
    currentEmailId: null,
    listScrollTop: 0,
    emailCache: {},            // id → full email with body (LRU, max 50)
};

const PAGE_SIZE = 50;
const CACHE_LIMIT = 200;  // Max emails kept in memory (per Fleury, lower than desktop)
const BODY_CACHE_LIMIT = 50;

// ============================================================================
// Date formatting (reused from desktop app.js)
// ============================================================================

function formatDate(isoString) {
    const date = new Date(isoString);
    const now = new Date();
    const diff = now - date;
    if (diff < 86400000 && date.getDate() === now.getDate()) {
        return date.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
    } else if (diff < 604800000) {
        return date.toLocaleDateString([], { weekday: 'short' });
    } else {
        return date.toLocaleDateString([], { month: 'short', day: 'numeric' });
    }
}

function getDateGroup(isoString) {
    const date = new Date(isoString);
    const now = new Date();
    const startOfToday = new Date(now.getFullYear(), now.getMonth(), now.getDate());
    const startOfYesterday = new Date(startOfToday);
    startOfYesterday.setDate(startOfYesterday.getDate() - 1);
    const startOfThisWeek = new Date(startOfToday);
    const dayOfWeek = startOfToday.getDay();
    const mondayOffset = dayOfWeek === 0 ? 6 : dayOfWeek - 1;
    startOfThisWeek.setDate(startOfThisWeek.getDate() - mondayOffset);
    const startOfLastWeek = new Date(startOfThisWeek);
    startOfLastWeek.setDate(startOfLastWeek.getDate() - 7);
    const startOfThisMonth = new Date(now.getFullYear(), now.getMonth(), 1);
    const startOfLastMonth = new Date(now.getFullYear(), now.getMonth() - 1, 1);
    if (date >= startOfToday) return 'Today';
    if (date >= startOfYesterday) return 'Yesterday';
    if (date >= startOfThisWeek) return 'This Week';
    if (date >= startOfLastWeek) return 'Last Week';
    if (date >= startOfThisMonth) return 'This Month';
    if (date >= startOfLastMonth) return 'Last Month';
    return 'Older';
}

function escapeHtml(text) {
    return text.replace(/&/g, '&amp;').replace(/</g, '&lt;')
               .replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}

// ============================================================================
// HTML sanitization — ported from static/app.js
// ============================================================================

function sanitizeStyleContent(css) {
    css = css.replace(/@import\b[^;]*;?/gi, '');
    css = css.replace(/@font-face\s*\{[^}]*\}/gi, '');
    css = css.replace(/url\s*\([^)]*\)/gi, '');
    css = css.replace(/expression\s*\([^)]*\)/gi, '');
    css = css.replace(/-moz-binding\s*:[^;]+;?/gi, '');
    css = css.replace(/behavior\s*:[^;]+;?/gi, '');
    return css;
}

function scopeStyleToEmailBody(css) {
    return css.replace(
        /([^{}@]+)\{/g,
        (match, selectors) => {
            if (selectors.trim().startsWith('@')) return match;
            const scoped = selectors.split(',')
                .map(s => s.trim())
                .filter(s => s.length > 0)
                .map(s => `#email-body ${s}`)
                .join(', ');
            return scoped + ' {';
        }
    );
}

const SAFE_DATA_IMAGE_PREFIXES = [
    'data:image/png', 'data:image/jpeg', 'data:image/gif', 'data:image/webp',
];

function sanitizeHtml(html) {
    const doc = new DOMParser().parseFromString(html, 'text/html');

    // Remove dangerous elements
    const dangerousTags = [
        'script', 'iframe', 'object', 'embed', 'form', 'input',
        'button', 'meta', 'base', 'link', 'svg', 'math',
    ];
    for (const tag of dangerousTags) {
        for (const el of doc.querySelectorAll(tag)) el.remove();
    }

    // Sanitize and scope style elements
    for (const el of doc.querySelectorAll('style')) {
        el.textContent = scopeStyleToEmailBody(sanitizeStyleContent(el.textContent));
    }

    // Sanitize all elements
    for (const el of doc.querySelectorAll('*')) {
        const attrs = [...el.attributes];
        for (const attr of attrs) {
            const name = attr.name.toLowerCase();
            const value = attr.value.toLowerCase().trim();

            // Remove event handlers
            if (name.startsWith('on')) {
                el.removeAttribute(attr.name);
                continue;
            }

            // Remove dangerous URL schemes
            if (['href', 'src', 'action', 'xlink:href', 'formaction'].includes(name)) {
                if (value.startsWith('javascript:') || value.startsWith('vbscript:')) {
                    el.removeAttribute(attr.name);
                }
                // Block data: URLs except safe image formats (SVG excluded — can contain script)
                if (value.startsWith('data:') &&
                    !SAFE_DATA_IMAGE_PREFIXES.some(p => value.startsWith(p))) {
                    el.removeAttribute(attr.name);
                }
            }

            // Remove dangerous style expressions
            if (name === 'style') {
                if (value.includes('expression') || value.includes('javascript')) {
                    el.removeAttribute(attr.name);
                }
            }
        }
    }

    // Linkify bare URLs in text nodes
    const walker = doc.createTreeWalker(doc.body, NodeFilter.SHOW_TEXT);
    const textNodes = [];
    while (walker.nextNode()) textNodes.push(walker.currentNode);
    for (const node of textNodes) {
        if (node.parentElement && node.parentElement.closest('a')) continue;
        const segments = segmentUrls(node.textContent);
        if (segments.length <= 1 && !segments[0]?.url) continue;
        const frag = doc.createDocumentFragment();
        for (const seg of segments) {
            if (seg.url) {
                const a = doc.createElement('a');
                a.href = seg.url;
                a.textContent = seg.url;
                a.setAttribute('target', '_blank');
                a.setAttribute('rel', 'noopener noreferrer');
                frag.appendChild(a);
            } else {
                frag.appendChild(doc.createTextNode(seg.text));
            }
        }
        node.parentNode.replaceChild(frag, node);
    }

    // Make all links open in a new tab
    for (const el of doc.querySelectorAll('a[href]')) {
        el.setAttribute('target', '_blank');
        el.setAttribute('rel', 'noopener noreferrer');
    }

    return doc.body.innerHTML;
}

function segmentUrls(text) {
    const re = /https?:\/\/[^\s<>"')\]]+/g;
    const parts = [];
    let last = 0, m;
    while ((m = re.exec(text)) !== null) {
        const url = m[0].replace(/[.,;:!?]+$/, '');
        if (m.index > last) parts.push({ text: text.slice(last, m.index) });
        parts.push({ text: url, url });
        last = m.index + url.length;
        re.lastIndex = last;
    }
    if (last < text.length) parts.push({ text: text.slice(last) });
    return parts;
}

function linkifyText(text) {
    return segmentUrls(text).map(p => p.url
        ? `<a href="${escapeHtml(p.url)}" target="_blank" rel="noopener noreferrer">${escapeHtml(p.url)}</a>`
        : escapeHtml(p.text)
    ).join('');
}

// ============================================================================
// Attachment rendering helpers
// ============================================================================

function formatFileSize(bytes) {
    if (bytes <= 0) return '0 B';
    const units = ['B', 'KB', 'MB', 'GB'];
    const i = Math.floor(Math.log(bytes) / Math.log(1024));
    const val = bytes / Math.pow(1024, i);
    return (i === 0 ? val : val.toFixed(1)) + ' ' + units[i];
}

function getFileIcon(mimeType, filename) {
    const ext = filename.split('.').pop()?.toLowerCase() || '';
    if (mimeType.startsWith('image/')) return '\u{1F5BC}';
    if (mimeType === 'application/pdf' || ext === 'pdf') return '\u{1F4C4}';
    if (mimeType.startsWith('audio/')) return '\u{1F3B5}';
    if (mimeType.startsWith('video/')) return '\u{1F3AC}';
    if (['zip', 'gz', 'tar', 'rar', '7z', 'bz2'].includes(ext)) return '\u{1F4E6}';
    if (['xls', 'xlsx', 'csv', 'ods'].includes(ext)) return '\u{1F4CA}';
    if (['doc', 'docx', 'odt', 'rtf'].includes(ext)) return '\u{1F4DD}';
    if (['ppt', 'pptx', 'odp'].includes(ext)) return '\u{1F4CA}';
    if (['txt', 'md', 'log'].includes(ext)) return '\u{1F4C3}';
    return '\u{1F4CE}';
}

// ============================================================================
// Email body cache (LRU, max 50)
// ============================================================================

function cacheEmail(email) {
    const keys = Object.keys(state.emailCache);
    if (keys.length >= BODY_CACHE_LIMIT) {
        delete state.emailCache[keys[0]];
    }
    state.emailCache[email.id] = email;
}

// ============================================================================
// Data loading
// ============================================================================

async function loadMailboxes() {
    state.mailboxes = await getMailboxes(state.session);
    const inbox = state.mailboxes.find(m => m.role === 'inbox');
    state.inboxId = inbox?.id || null;
}

async function loadIdentities() {
    state.identities = await getIdentities(state.session);
}

async function loadEmails(refresh = false) {
    if (state.loading) return;
    state.loading = true;
    showStatus('Loading...');
    try {
        const ids = await queryEmails(state.session, state.inboxId, PAGE_SIZE, 0);
        if (ids.length) {
            state.emails = await getEmails(state.session, ids);
        } else {
            state.emails = [];
        }
        renderEmailList();
        showStatus('');
    } catch (err) {
        if (err instanceof JmapAuthError) {
            clearSession();
            showLogin();
            return;
        }
        showStatus('Failed to load: ' + err.message);
    } finally {
        state.loading = false;
        finishPullRefresh();
    }
}

async function loadMoreEmails() {
    if (state.loadingMore || state.emails.length >= CACHE_LIMIT) return;
    state.loadingMore = true;
    try {
        const ids = await queryEmails(
            state.session, state.inboxId,
            PAGE_SIZE, state.emails.length
        );
        if (ids.length) {
            const existingIds = new Set(state.emails.map(e => e.id));
            const newIds = ids.filter(id => !existingIds.has(id));
            if (newIds.length) {
                const newEmails = await getEmails(state.session, newIds);
                state.emails = state.emails.concat(newEmails);
                renderEmailList();
            }
        }
    } catch (err) {
        // Silently fail on refill — not critical
    } finally {
        state.loadingMore = false;
    }
}

// ============================================================================
// Rendering
// ============================================================================

function renderEmailList() {
    const listEl = document.getElementById('email-list');
    if (!state.emails.length) {
        listEl.innerHTML = '<div class="empty-state">No emails</div>';
        return;
    }

    let lastGroup = null;
    listEl.innerHTML = state.emails.map(email => {
        const from = email.from[0];
        const fromDisplay = from?.name || from?.email || 'Unknown';
        const date = formatDate(email.receivedAt);
        const group = getDateGroup(email.receivedAt);
        let divider = '';
        if (group !== lastGroup) {
            lastGroup = group;
            divider = '<div class="date-divider"><span>' + escapeHtml(group) + '</span></div>';
        }
        return divider +
            '<div class="email-row' + (email.isUnread ? ' unread' : '') + '" data-id="' + escapeHtml(email.id) + '">' +
                '<div class="email-row-main">' +
                    '<div class="email-row-top">' +
                        '<span class="email-from">' + escapeHtml(fromDisplay) + '</span>' +
                        '<span class="email-date">' + date + '</span>' +
                    '</div>' +
                    '<div class="email-subject">' + escapeHtml(email.subject) + '</div>' +
                    '<div class="email-preview">' + escapeHtml(email.preview) + '</div>' +
                '</div>' +
                '<div class="email-row-indicators">' +
                    (email.isFlagged ? '<span class="star">★</span>' : '') +
                    (email.hasAttachment ? '<span class="attach">📎</span>' : '') +
                '</div>' +
            '</div>';
    }).join('');
}

function showStatus(msg) {
    const el = document.getElementById('status-bar');
    if (el) el.textContent = msg;
}

// ============================================================================
// Email detail view
// ============================================================================

async function showEmail(emailId) {
    state.currentView = 'detail';
    state.currentEmailId = emailId;
    state.listScrollTop = document.getElementById('email-list-wrap').scrollTop;

    document.getElementById('email-list-wrap').style.display = 'none';
    document.getElementById('app-header').style.display = 'none';
    document.getElementById('email-detail').style.display = 'flex';

    history.pushState({ view: 'detail', emailId }, '');

    // Render partial detail from list data immediately
    const listEmail = state.emails.find(e => e.id === emailId);
    if (listEmail) renderEmailDetailPartial(listEmail);

    // Full body: use cache or fetch
    let full = state.emailCache[emailId];
    if (!full) {
        try {
            const fetched = await getEmails(state.session, [emailId], true);
            if (fetched.length) {
                full = fetched[0];
                cacheEmail(full);
            }
        } catch (err) {
            if (err instanceof JmapAuthError) {
                clearSession();
                showLogin();
                return;
            }
            document.getElementById('email-body').innerHTML =
                '<div style="padding:16px;color:var(--text-muted)">Failed to load email body.</div>';
            return;
        }
    }

    if (full) renderEmailDetail(full);

    // Auto-mark-read
    if (listEmail?.isUnread) {
        markRead(state.session, emailId).catch(() => {});
        // Update local state
        if (listEmail) {
            listEmail.isUnread = false;
            listEmail.keywords['$seen'] = true;
        }
        if (full) {
            full.isUnread = false;
            full.keywords['$seen'] = true;
        }
    }

    // Prefetch next emails
    prefetchAdjacentEmails(emailId);
}

function renderEmailDetailPartial(email) {
    const from = email.from[0];
    const fromDisplay = from?.name
        ? `${escapeHtml(from.name)} <${escapeHtml(from.email)}>`
        : escapeHtml(from?.email || 'Unknown');

    document.getElementById('detail-subject').textContent = email.subject;
    document.getElementById('detail-from').innerHTML = fromDisplay;
    document.getElementById('detail-date').textContent = formatDetailDate(email.receivedAt);
    document.getElementById('detail-recipients').innerHTML = formatRecipients(email);
    document.getElementById('detail-attachments').innerHTML = '';
    document.getElementById('detail-calendar').innerHTML = '';
    document.getElementById('email-body').innerHTML =
        '<div style="padding:16px;color:var(--text-muted)">Loading...</div>';
}

function renderEmailDetail(email) {
    const from = email.from[0];
    const fromDisplay = from?.name
        ? `${escapeHtml(from.name)} <${escapeHtml(from.email)}>`
        : escapeHtml(from?.email || 'Unknown');

    document.getElementById('detail-subject').textContent = email.subject;
    document.getElementById('detail-from').innerHTML = fromDisplay;
    document.getElementById('detail-date').textContent = formatDetailDate(email.receivedAt);
    document.getElementById('detail-recipients').innerHTML = formatRecipients(email);

    // Attachments
    const attachments = email.attachments || [];
    if (attachments.length) {
        document.getElementById('detail-attachments').innerHTML = renderAttachments(attachments);
    } else {
        document.getElementById('detail-attachments').innerHTML = '';
    }

    // Calendar indicator
    if (email.hasCalendar) {
        document.getElementById('detail-calendar').innerHTML =
            '<div class="calendar-indicator">This email contains a calendar invitation</div>';
    } else {
        document.getElementById('detail-calendar').innerHTML = '';
    }

    // Body
    if (email.htmlBody) {
        document.getElementById('email-body').innerHTML = sanitizeHtml(email.htmlBody);
    } else if (email.textBody) {
        document.getElementById('email-body').innerHTML =
            '<div class="plain-text-body">' + linkifyText(email.textBody) + '</div>';
    } else {
        document.getElementById('email-body').innerHTML =
            '<div style="padding:16px;color:var(--text-muted)">No content</div>';
    }
}

function renderAttachments(attachments) {
    const header = '<div class="att-header">Attachments (' + attachments.length + ')</div>';
    const items = attachments.map(att => {
        const icon = getFileIcon(att.mimeType, att.name);
        const size = formatFileSize(att.size);
        const url = blobUrl(state.session, att.blobId, att.name, att.mimeType);
        return '<a class="att-item" href="' + escapeHtml(url) + '" target="_blank" rel="noopener noreferrer">' +
            '<span class="att-icon">' + icon + '</span>' +
            '<span class="att-name">' + escapeHtml(att.name) + '</span>' +
            '<span class="att-size">' + size + '</span>' +
            '</a>';
    }).join('');
    return header + items;
}

function formatDetailDate(isoString) {
    const d = new Date(isoString);
    return d.toLocaleDateString([], { weekday: 'short', month: 'short', day: 'numeric', year: 'numeric' })
        + ' ' + d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
}

function formatRecipients(email) {
    const parts = [];
    if (email.to?.length) {
        parts.push('To: ' + email.to.map(a => escapeHtml(a.name || a.email)).join(', '));
    }
    if (email.cc?.length) {
        parts.push('Cc: ' + email.cc.map(a => escapeHtml(a.name || a.email)).join(', '));
    }
    return parts.join('<br>');
}

function showList() {
    state.currentView = 'list';
    state.currentEmailId = null;

    document.getElementById('email-detail').style.display = 'none';
    document.getElementById('email-list-wrap').style.display = '';
    document.getElementById('app-header').style.display = '';

    // Restore scroll position
    document.getElementById('email-list-wrap').scrollTop = state.listScrollTop;

    // Re-render list to update read state
    renderEmailList();
}

function prefetchAdjacentEmails(emailId) {
    const idx = state.emails.findIndex(e => e.id === emailId);
    if (idx === -1) return;
    const toFetch = [];
    for (let i = 1; i <= 3; i++) {
        const next = state.emails[idx + i];
        if (next && !state.emailCache[next.id]) toFetch.push(next.id);
    }
    if (!toFetch.length) return;
    getEmails(state.session, toFetch, true)
        .then(emails => { for (const e of emails) cacheEmail(e); })
        .catch(() => {});
}

// ============================================================================
// UI transitions
// ============================================================================

function showApp(session) {
    state.session = session;
    document.getElementById('login-screen').classList.add('hidden');
    document.getElementById('app-shell').classList.add('active');
    // Load data
    Promise.all([loadMailboxes(), loadIdentities()])
        .then(() => loadEmails())
        .catch(err => showStatus('Failed: ' + err.message));
}

function showLogin() {
    state.session = null;
    document.getElementById('app-shell').classList.remove('active');
    document.getElementById('login-screen').classList.remove('hidden');
}

// ============================================================================
// Pull-to-refresh
// ============================================================================

let pullStartY = 0;
let pulling = false;

function setupPullToRefresh() {
    const listWrap = document.getElementById('email-list-wrap');
    if (!listWrap) return;

    listWrap.addEventListener('touchstart', (e) => {
        if (listWrap.scrollTop === 0) {
            pullStartY = e.touches[0].clientY;
            pulling = true;
        }
    }, { passive: true });

    listWrap.addEventListener('touchmove', (e) => {
        if (!pulling) return;
        const dy = e.touches[0].clientY - pullStartY;
        if (dy > 0) {
            e.preventDefault();
            const indicator = document.getElementById('pull-indicator');
            if (dy < 120) {
                indicator.style.height = dy + 'px';
                indicator.style.opacity = Math.min(dy / 60, 1);
            }
        }
    }, { passive: false });

    listWrap.addEventListener('touchend', () => {
        if (!pulling) return;
        pulling = false;
        const indicator = document.getElementById('pull-indicator');
        const h = parseInt(indicator.style.height) || 0;
        if (h > 60) {
            indicator.style.height = '40px';
            indicator.textContent = 'Refreshing...';
            loadEmails(true);
        } else {
            finishPullRefresh();
        }
    });
}

function finishPullRefresh() {
    const indicator = document.getElementById('pull-indicator');
    if (indicator) {
        indicator.style.height = '0';
        indicator.style.opacity = '0';
        indicator.textContent = 'Pull to refresh';
    }
}

// ============================================================================
// Infinite scroll
// ============================================================================

function setupInfiniteScroll() {
    const listWrap = document.getElementById('email-list-wrap');
    if (!listWrap) return;
    listWrap.addEventListener('scroll', () => {
        const { scrollTop, scrollHeight, clientHeight } = listWrap;
        if (scrollHeight - scrollTop - clientHeight < 200) {
            loadMoreEmails();
        }
    }, { passive: true });
}

// ============================================================================
// Boot
// ============================================================================

async function init() {
    const session = getSession();
    if (session) {
        try {
            const fresh = await connect(session.username, session.token);
            saveSession(fresh);
            showApp(fresh);
        } catch (err) {
            if (err instanceof JmapAuthError) {
                clearSession();
                showLogin();
            } else {
                // Network error — use saved session (supports offline launch)
                showApp(session);
            }
        }
    } else {
        showLogin();
    }
}

// Login handler
document.getElementById('login-btn').addEventListener('click', async () => {
    const username = document.getElementById('login-username').value.trim();
    const token = document.getElementById('login-token').value.trim();
    const errorEl = document.getElementById('login-error');
    errorEl.textContent = '';
    if (!username || !token) {
        errorEl.textContent = 'Both fields are required.';
        return;
    }
    const btn = document.getElementById('login-btn');
    btn.disabled = true;
    btn.textContent = 'Connecting...';
    try {
        const session = await connect(username, token);
        saveSession(session);
        showApp(session);
    } catch (err) {
        errorEl.textContent = err.message;
    } finally {
        btn.disabled = false;
        btn.textContent = 'Connect';
    }
});

// Logout handler
document.getElementById('logout-btn').addEventListener('click', () => {
    clearSession();
    showLogin();
});

// Back button (detail → list)
document.getElementById('back-btn').addEventListener('click', () => showList());

// Email row click → detail view
document.getElementById('email-list').addEventListener('click', (e) => {
    const row = e.target.closest('.email-row');
    if (!row) return;
    const id = row.dataset.id;
    if (id) showEmail(id);
});

// Browser back button
window.addEventListener('popstate', (e) => {
    if (state.currentView === 'detail') {
        showList();
    }
});

// Replace initial history state
history.replaceState({ view: 'list' }, '');

// Register service worker
if ('serviceWorker' in navigator) {
    navigator.serviceWorker.register('/mobile/sw.js', { scope: '/mobile/' })
        .catch(() => {});
}

setupPullToRefresh();
setupInfiniteScroll();
init();

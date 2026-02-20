// Supervillain — Mobile app logic
// Email list view with pull-to-refresh and infinite scroll.

import {
    connect, getSession, saveSession, clearSession,
    getMailboxes, getIdentities, queryEmails, getEmails,
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
};

const PAGE_SIZE = 50;
const CACHE_LIMIT = 200;  // Max emails kept in memory (per Fleury, lower than desktop)

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
    const div = document.createElement('div');
    div.textContent = text;
    return div.innerHTML;
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

// Register service worker
if ('serviceWorker' in navigator) {
    navigator.serviceWorker.register('/mobile/sw.js', { scope: '/mobile/' })
        .catch(() => {});
}

setupPullToRefresh();
setupInfiniteScroll();
init();

// Supervillain — Mobile app logic
// Email list view backed by the server /api/* routes (shared client in
// /api.js, loaded as a classic script before this module — makeApi,
// ApiError, and ApiAuthError are globals).

// ============================================================================
// State
// ============================================================================

const state = {
    accounts: [],
    currentAccount: null,
    api: null,                 // makeApi(currentAccount.id)
    mailboxes: [],
    inboxId: null,
    emails: [],
    loading: false,
    loadingMore: false,
    currentView: 'list',      // 'list' | 'detail'
    currentEmailId: null,
    listScrollTop: 0,
    emailCache: {},            // id → full email with body (LRU, max 50)
    lastRenderedGroup: null,   // date-divider continuity for append-only pages
};

// Unscoped instance for global routes (/accounts).
const apiGlobal = makeApi(null);

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
// Error surface — every failed API call lands here so failures are visible
// on a phone without devtools. Auth failures get a distinct message since
// re-authorization happens in desktop Settings, not on the phone.
// ============================================================================

let errorToastTimer = null;

function showError(context, err) {
    const el = document.getElementById('error-toast');
    if (!el) return;
    const detail = err instanceof ApiAuthError
        ? 'account needs re-authorization (open Settings on desktop)'
        : err.message;
    el.textContent = context + ': ' + detail;
    el.classList.remove('hidden');
    if (errorToastTimer) clearTimeout(errorToastTimer);
    errorToastTimer = setTimeout(() => el.classList.add('hidden'), 6000);
}

// ============================================================================
// Email-body rendering — sandboxed iframe
// ============================================================================
// Attacker-controlled email HTML goes into a sandboxed iframe with neither
// allow-scripts nor allow-same-origin: scripts in the iframe never run, so
// the entire class of HTML-sanitizer bypasses (mXSS, scheme tricks,
// namespace confusion) cannot reach the app origin. allow-popups +
// allow-popups-to-escape-sandbox lets recipient links open new tabs as a
// normal browsing context; <base target=_blank> in the srcdoc makes that
// the default.

function renderHtmlBodyIframe(container, html) {
    container.replaceChildren();
    const iframe = document.createElement('iframe');
    iframe.setAttribute('sandbox', 'allow-popups allow-popups-to-escape-sandbox');
    iframe.className = 'email-iframe';
    iframe.setAttribute('srcdoc', wrapEmailHtml(linkifyHtml(html), isDarkTheme()));
    container.appendChild(iframe);
}

// Mobile follows the OS color scheme (prefers-color-scheme media query in
// the page <style>). matchMedia gives us the same signal to mirror inside
// the iframe so the email doesn't render bright in a dark UI.
function isDarkTheme() {
    return window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)').matches;
}

// Walk text nodes outside <a> and wrap bare https?:// URLs in <a>. Purely
// cosmetic — the iframe sandbox is the security boundary, not this function.
function linkifyHtml(html) {
    const doc = new DOMParser().parseFromString(html, 'text/html');
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
    return doc.body.innerHTML;
}

function wrapEmailHtml(html, dark) {
    const bg = dark ? '#1a1a2e' : '#fff';
    const fg = dark ? '#e0e0e0' : '#222';
    const linkColor = dark ? '#e94560' : '#e94560';
    const quoteBorder = dark ? '#444' : '#ddd';
    const quoteFg = dark ? '#999' : '#666';
    return '<!doctype html><html><head>'
        + '<meta charset="utf-8">'
        + '<meta name="viewport" content="width=device-width, initial-scale=1">'
        + '<base target="_blank">'
        + '<meta name="color-scheme" content="' + (dark ? 'dark' : 'light') + '">'
        + '<style>'
        + 'html,body{margin:0;padding:12px;background:' + bg + ';color:' + fg + ';'
        + 'font-family:-apple-system,BlinkMacSystemFont,"SF Pro Text",system-ui,sans-serif;'
        + 'font-size:15px;line-height:1.5;word-wrap:break-word;overflow-wrap:break-word;}'
        + 'img{max-width:100%;height:auto;}'
        + 'table{max-width:100%;overflow-x:auto;display:block;}'
        + 'pre{white-space:pre-wrap;overflow-x:auto;}'
        + 'a{color:' + linkColor + ';}'
        + 'blockquote{border-left:3px solid ' + quoteBorder + ';margin:8px 0;padding:4px 12px;color:' + quoteFg + ';}'
        + '*{writing-mode: horizontal-tb !important;text-orientation: mixed !important;}'
        + '</style>'
        + '</head><body>'
        + html
        + '</body></html>';
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

// Server attachment route; scoped to the current account explicitly since
// this URL lands in an <a href>, not an api() call.
function attachmentUrl(emailId, att) {
    return '/api/emails/' + encodeURIComponent(emailId)
        + '/attachments/' + encodeURIComponent(att.blob_id)
        + '/' + encodeURIComponent(att.name)
        + '?account=' + encodeURIComponent(state.currentAccount.id);
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
// Accounts
// ============================================================================

async function loadAccounts() {
    const data = await apiGlobal('GET', '/accounts');
    state.accounts = data.accounts || [];
    const nonSetupErrors = (data.errors || []).filter(e => e.provider !== 'setup');
    if (nonSetupErrors.length) {
        showError('Accounts', new Error(nonSetupErrors.map(e => e.provider + ': ' + e.message).join('; ')));
    }
}

function connectedAccounts() {
    return state.accounts.filter(a => a.authStatus !== 'pending');
}

function selectAccount(account) {
    state.currentAccount = account;
    state.api = makeApi(account.id);
    state.mailboxes = [];
    state.inboxId = null;
    state.emails = [];
    state.emailCache = {};
    state.lastRenderedGroup = null;
    state.listScrollTop = 0;
    renderAccountButton();
    hideAccountPicker();
    loadMailboxes()
        .then(() => loadEmails())
        .catch(err => showError('Load mailboxes', err));
}

function renderAccountButton() {
    const btn = document.getElementById('account-btn');
    if (!btn) return;
    btn.textContent = state.currentAccount ? state.currentAccount.email : 'No account';
}

function renderAccountPicker() {
    const list = document.getElementById('account-picker-list');
    list.innerHTML = state.accounts.map(a => {
        const pending = a.authStatus === 'pending';
        const current = state.currentAccount && a.id === state.currentAccount.id;
        const cls = 'account-row' + (current ? ' current' : '') + (pending ? ' pending' : '');
        return '<button class="' + cls + '" data-id="' + escapeHtml(a.id) + '">'
            + '<span>' + escapeHtml(a.email || a.id) + (pending ? ' (needs authorization)' : '') + '</span>'
            + '<span class="account-provider">' + escapeHtml(a.provider || '') + '</span>'
            + '</button>';
    }).join('');
}

function showAccountPicker() {
    renderAccountPicker();
    document.getElementById('account-picker').classList.remove('hidden');
}

function hideAccountPicker() {
    document.getElementById('account-picker').classList.add('hidden');
}

// ============================================================================
// Data loading
// ============================================================================

async function loadMailboxes() {
    state.mailboxes = await state.api('GET', '/mailboxes');
    const inbox = state.mailboxes.find(m => m.role === 'inbox');
    state.inboxId = inbox?.id || null;
}

function emailListPath(offset) {
    let path = '/emails?limit=' + PAGE_SIZE;
    if (state.inboxId) path += '&mailbox_id=' + encodeURIComponent(state.inboxId);
    if (offset > 0) path += '&offset=' + offset;
    return path;
}

async function loadEmails() {
    if (state.loading) return;
    state.loading = true;
    showStatus('Loading...');
    try {
        state.emails = await state.api('GET', emailListPath(0));
        renderEmailList();
        showStatus('');
    } catch (err) {
        showStatus('');
        showError('Load emails', err);
    } finally {
        state.loading = false;
        finishPullRefresh();
    }
}

async function loadMoreEmails() {
    if (state.loadingMore || state.loading) return;
    if (!state.emails.length || state.emails.length >= CACHE_LIMIT) return;
    state.loadingMore = true;
    try {
        const page = await state.api('GET', emailListPath(state.emails.length));
        const existingIds = new Set(state.emails.map(e => e.id));
        const newEmails = page.filter(e => !existingIds.has(e.id));
        if (newEmails.length) {
            state.emails = state.emails.concat(newEmails);
            appendEmailRows(newEmails);
        }
    } catch (err) {
        showError('Load more', err);
    } finally {
        state.loadingMore = false;
    }
}

// ============================================================================
// Rendering
// ============================================================================

// Renders rows for `emails`, threading the date-divider group through
// `startGroup` so appended pages continue the sequence instead of
// repeating a divider. Returns the HTML and the group the sequence ended on.
function renderEmailRows(emails, startGroup) {
    let lastGroup = startGroup;
    const html = emails.map(email => {
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
    return { html, lastGroup };
}

function renderEmailList() {
    const listEl = document.getElementById('email-list');
    if (!state.emails.length) {
        listEl.innerHTML = '<div class="empty-state">No emails</div>';
        state.lastRenderedGroup = null;
        return;
    }
    const { html, lastGroup } = renderEmailRows(state.emails, null);
    listEl.innerHTML = html;
    state.lastRenderedGroup = lastGroup;
}

// Append-only pagination: renders just the new page and inserts it at the
// end, instead of rebuilding the full list via innerHTML each time.
function appendEmailRows(newEmails) {
    const listEl = document.getElementById('email-list');
    const { html, lastGroup } = renderEmailRows(newEmails, state.lastRenderedGroup);
    listEl.insertAdjacentHTML('beforeend', html);
    state.lastRenderedGroup = lastGroup;
}

function showStatus(msg) {
    const el = document.getElementById('status-bar');
    if (el) el.textContent = msg;
}

// ============================================================================
// Email detail view
// ============================================================================

async function showEmail(emailId) {
    // Only pushState when transitioning from list; replaceState when navigating
    // between emails to avoid unbounded history growth.
    if (state.currentView === 'detail') {
        history.replaceState({ view: 'detail', emailId }, '');
    } else {
        state.listScrollTop = document.getElementById('email-list-wrap').scrollTop;
        history.pushState({ view: 'detail', emailId }, '');
    }

    state.currentView = 'detail';
    state.currentEmailId = emailId;

    document.getElementById('email-list-wrap').style.display = 'none';
    document.getElementById('app-header').style.display = 'none';
    document.getElementById('email-detail').style.display = 'flex';

    // Render partial detail from list data immediately
    const listEmail = state.emails.find(e => e.id === emailId);
    if (listEmail) renderEmailDetailPartial(listEmail);

    // Full body: use cache or fetch (delete+reinsert to promote in FIFO-with-promotion cache)
    let full = state.emailCache[emailId];
    if (full) {
        delete state.emailCache[emailId];
        state.emailCache[emailId] = full;
    }
    if (!full) {
        try {
            full = await state.api('GET', '/emails/' + encodeURIComponent(emailId));
            cacheEmail(full);
        } catch (err) {
            showError('Load email', err);
            document.getElementById('email-body').innerHTML =
                '<div style="padding:16px;color:var(--text-muted)">Failed to load email body.</div>';
            return;
        }
    }

    renderEmailDetail(full);

    // The server auto-marks read on GET /emails/{id}; mirror it locally.
    if (listEmail?.isUnread) listEmail.isUnread = false;
    if (full.isUnread) full.isUnread = false;

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
    renderEmailDetailPartial(email);

    // Attachments
    const attachments = email.attachments || [];
    if (attachments.length) {
        document.getElementById('detail-attachments').innerHTML = renderAttachments(attachments, email.id);
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
    const bodyEl = document.getElementById('email-body');
    if (email.htmlBody) {
        bodyEl.classList.add('html-content');
        renderHtmlBodyIframe(bodyEl, email.htmlBody);
    } else if (email.textBody) {
        bodyEl.classList.remove('html-content');
        bodyEl.innerHTML = '<div class="plain-text-body">' + linkifyText(email.textBody) + '</div>';
    } else {
        bodyEl.classList.remove('html-content');
        bodyEl.innerHTML = '<div style="padding:16px;color:var(--text-muted)">No content</div>';
    }
}

function renderAttachments(attachments, emailId) {
    const header = '<div class="att-header">Attachments (' + attachments.length + ')</div>';
    const items = attachments.map(att => {
        const icon = getFileIcon(att.mime_type, att.name);
        const size = formatFileSize(att.size);
        const url = attachmentUrl(emailId, att);
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
    // Background warm-up only — a failure here costs nothing the user can
    // see, so log instead of toasting.
    Promise.all(toFetch.map(id =>
        state.api('GET', '/emails/' + encodeURIComponent(id)).then(e => cacheEmail(e))
    )).catch(err => console.warn('Prefetch failed:', err));
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
            loadEmails();
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
    // Scrub the pre-rewire Fastmail bearer token off installed PWAs — the
    // direct-JMAP client (and its localStorage session) no longer exists.
    localStorage.removeItem('supervillain_session');

    document.getElementById('app-shell').classList.add('active');

    try {
        await loadAccounts();
    } catch (err) {
        showError('Load accounts', err);
        showStatus('Cannot reach server');
        return;
    }

    const connected = connectedAccounts();
    const defaultAcc = connected.find(a => a.isDefault) || connected[0];
    if (defaultAcc) {
        selectAccount(defaultAcc);
    } else {
        renderAccountButton();
        showStatus(state.accounts.length
            ? 'No authorized accounts — authorize in desktop Settings'
            : 'No accounts configured — add one in desktop Settings');
    }
}

// Account switcher
document.getElementById('account-btn').addEventListener('click', showAccountPicker);
document.getElementById('account-picker').addEventListener('click', (e) => {
    const row = e.target.closest('.account-row');
    if (!row) {
        // Tap on the backdrop dismisses
        if (e.target.id === 'account-picker') hideAccountPicker();
        return;
    }
    const account = state.accounts.find(a => a.id === row.dataset.id);
    if (!account) return;
    if (account.authStatus === 'pending') {
        showError('Switch account', new Error('account needs authorization (open Settings on desktop)'));
        return;
    }
    if (state.currentAccount && account.id === state.currentAccount.id) {
        hideAccountPicker();
        return;
    }
    selectAccount(account);
});

// Back button (detail → list) — use history.back() to pop the pushState entry
document.getElementById('back-btn').addEventListener('click', () => history.back());

// Email row click → detail view
document.getElementById('email-list').addEventListener('click', (e) => {
    const row = e.target.closest('.email-row');
    if (!row) return;
    const id = row.dataset.id;
    if (id) showEmail(id);
});

// Browser back button
window.addEventListener('popstate', (e) => {
    if (e.state?.view === 'detail' && state.currentView !== 'detail') {
        // Forward navigation to detail (not currently used, but safe)
    } else if (state.currentView === 'detail') {
        showList();
    }
});

// Replace initial history state
history.replaceState({ view: 'list' }, '');

// Register service worker. Skipped outside a secure context (plain http
// on anything but localhost) since registration there always fails —
// the serviceWorker API isn't exposed at all in that case.
if ('serviceWorker' in navigator) {
    if (window.isSecureContext) {
        navigator.serviceWorker.register('/mobile/sw.js', { scope: '/mobile/' })
            .catch((err) => console.warn('Service worker registration failed:', err));
    } else {
        console.info('Skipping service worker registration: not a secure context');
    }
}

setupPullToRefresh();
setupInfiniteScroll();
init();

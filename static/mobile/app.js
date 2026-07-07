// Supervillain — Mobile app logic
// Email list view backed by the server /api/* routes (shared client in
// /api.js, loaded as a classic script before this module — makeApi,
// ApiError, and ApiAuthError are globals).

// ============================================================================
// Screen model
// ============================================================================
// Flat set of full-screen views. setScreen() owns every show/hide, so adding
// a screen later (compose, mailboxes, search) is a new enum member plus one
// switch case — never another scattered display toggle.

const Screen = { LIST: 'list', DETAIL: 'detail', COMPOSE: 'compose' };

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
    loadAbort: null,           // AbortController for in-flight loadEmails/loadMoreEmails
    screen: Screen.LIST,       // Screen.LIST | Screen.DETAIL | Screen.COMPOSE
    currentEmailId: null,
    listScrollTop: 0,
    emailCache: {},            // id → full email with body (LRU, max 50)
    lastRenderedGroup: null,   // date-divider continuity for append-only pages
    undoStack: [],             // [{ action: 'archive'|'trash', email, index, mailboxId, settled }], capped at UNDO_STACK_LIMIT — see performUndo
    identities: [],            // [{ email, name }] for the current account's From selector
    identitiesAccount: null,   // account id the cached identities belong to
    replyContext: null,        // { inReplyTo?, quotedHtml, quotedText } appended at send time
    sending: false,            // in-flight send lock (disables Send, ignores re-taps)
};

// Unscoped instance for global routes (/accounts).
const apiGlobal = makeApi(null);

const PAGE_SIZE = 50;
const CACHE_LIMIT = 200;  // Max emails kept in memory (per Fleury, lower than desktop)
const BODY_CACHE_LIMIT = 50;
const UNDO_STACK_LIMIT = 10;

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

// Bottom toast shared by showError (failures) and transient status like the
// post-send "Sent" confirmation. One element, one timer — a newer message
// always replaces the pending one.
function showToast(message, duration = 6000) {
    const el = document.getElementById('error-toast');
    if (!el) return;
    el.textContent = message;
    el.classList.remove('hidden');
    if (errorToastTimer) clearTimeout(errorToastTimer);
    errorToastTimer = setTimeout(() => el.classList.add('hidden'), duration);
}

function showError(context, err) {
    const detail = err instanceof ApiAuthError
        ? 'account needs re-authorization (open Settings on desktop)'
        : err.message;
    showToast(context + ': ' + detail);
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
    // Cancel any in-flight load from the account we're leaving — otherwise
    // a slow response can land after the switch and overwrite the new
    // account's list (or get swallowed by the state.loading guard).
    state.loadAbort?.abort();
    state.loadAbort = new AbortController();
    state.currentAccount = account;
    state.api = makeApi(account.id);
    state.mailboxes = [];
    state.inboxId = null;
    state.emails = [];
    state.emailCache = {};
    state.lastRenderedGroup = null;
    state.listScrollTop = 0;
    // Identities are account-scoped; drop the previous account's set and
    // re-fetch for the From selector. A failure only disables the selector
    // (compose still works), so it's fire-and-forget with its own error sink.
    state.identities = [];
    state.identitiesAccount = null;
    loadIdentities(account.id);
    // Switching accounts drops any open detail view; without this the app
    // stays on a stale email from the account we just left.
    if (state.screen !== Screen.LIST) {
        setScreen(Screen.LIST);
        // setScreen() doesn't touch history — without this, history.state
        // still carries the old account's {screen:'detail', emailId} and
        // Back could pop straight onto a stale cross-account detail view.
        history.replaceState({ screen: Screen.LIST }, '');
    }
    renderAccountButton();
    hideAccountPicker();
    // Captured up front, mirroring loadEmails/loadMoreEmails: a /mailboxes
    // response landing after a second switch belongs to an account we've
    // already navigated away from and must not be applied.
    const acct = account.id;
    loadMailboxes(acct, state.loadAbort.signal)
        .then(() => {
            if (state.currentAccount?.id !== acct) return;
            return loadEmails();
        })
        .catch(err => {
            if (err.name === 'AbortError') return;
            showError('Load mailboxes', err);
        });
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

async function loadMailboxes(acct, signal) {
    const mailboxes = await state.api('GET', '/mailboxes', null, signal);
    // The account may have changed again while this was in flight (a second
    // switch aborts the signal above, but a response that lands in the same
    // tick as abort() can still resolve) — don't clobber the new account's
    // freshly-reset state with data that belongs to the one we left.
    if (state.currentAccount?.id !== acct) return;
    state.mailboxes = mailboxes;
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
    // No account selected yet (e.g. no accounts configured) — nothing to
    // load. Without this guard the call below throws 'state.api is not a
    // function', and the resulting toast wipes the 'No accounts configured'
    // status right after init() sets it.
    if (!state.api) {
        finishPullRefresh();
        return;
    }
    if (state.loading) {
        finishPullRefresh();
        return;
    }
    state.loading = true;
    showStatus('Loading...');
    // Captured up front: if the account changes before this resolves, the
    // response belongs to an account we've already navigated away from.
    const acct = state.currentAccount.id;
    try {
        const emails = await state.api('GET', emailListPath(0), null, state.loadAbort?.signal);
        if (state.currentAccount?.id !== acct) return;
        state.emails = emails;
        renderEmailList();
        showStatus('');
    } catch (err) {
        if (err.name === 'AbortError') return;
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
    const acct = state.currentAccount.id;
    try {
        const page = await state.api('GET', emailListPath(state.emails.length), null, state.loadAbort?.signal);
        if (state.currentAccount?.id !== acct) return;
        const existingIds = new Set(state.emails.map(e => e.id));
        const newEmails = page.filter(e => !existingIds.has(e.id));
        if (newEmails.length) {
            state.emails = state.emails.concat(newEmails);
            appendEmailRows(newEmails);
        }
    } catch (err) {
        if (err.name === 'AbortError') return;
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
//
// Each row is wrapped in `.email-row-wrap` with a `.swipe-bg` sibling behind
// it — the rowSwipeRecognizer (gesture controller, below) translates the
// `.email-row` and toggles which half of `.swipe-bg` is visible underneath.
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
            '<div class="email-row-wrap">' +
                '<div class="swipe-bg" aria-hidden="true">' +
                    '<span class="swipe-icon-archive">\u{1F5C4}</span>' +
                    '<span class="swipe-icon-trash">\u{1F5D1}</span>' +
                '</div>' +
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
// Email actions
// ============================================================================
// Optimistic updates mirroring desktop's emailAction/toggleUnread/toggleFlag
// (static/app.js): mutate state.emails — and the cached detail body, when
// present — immediately, then reconcile with the server. A failure reverts
// the mutation and reports through showError, the only failure sink on a
// phone without devtools.

// Archive/trash push onto this so a later undo can restore them; capped so a
// long swiping session can't grow it unbounded. Returns the entry so a
// failed action can retract exactly its own push (see emailAction's catch).
function pushUndo(action, email, index) {
    const entry = { action, email, index, mailboxId: state.inboxId };
    state.undoStack.push(entry);
    capUndoStack();
    showUndoToast(entry);
    return entry;
}

// Single owner of the cap so every push site (pushUndo above, performUndo's
// failure re-push below) enforces it — a bare push would bypass the limit.
function capUndoStack() {
    if (state.undoStack.length > UNDO_STACK_LIMIT) state.undoStack.shift();
}

// ============================================================================
// Undo toast
// ============================================================================
// v1 UI surfaces only the single most recent undo — the stack still holds up
// to UNDO_STACK_LIMIT entries for a future keyboard-style multi-undo, but a
// new archive/trash always replaces the pending toast. undoToastEntry tracks
// which stack entry the visible toast represents, so a failure elsewhere
// (emailAction's catch) can tell whether it's still the one showing before
// hiding it out from under a newer action.

let undoToastTimer = null;
let undoToastEntry = null;

function showUndoToast(entry) {
    const el = document.getElementById('undo-toast');
    if (!el) return;
    undoToastEntry = entry;
    el.textContent = (entry.action === 'archive' ? 'Email archived' : 'Email trashed') + ' — Undo';
    el.classList.remove('hidden');
    if (undoToastTimer) clearTimeout(undoToastTimer);
    undoToastTimer = setTimeout(() => hideUndoToast(entry), 5000);
}

// No-op unless `entry` is still the one the toast is showing — callers pass
// the entry they know about so a stale hide (e.g. a failed action whose
// toast a newer action already replaced) can't clobber the current toast.
function hideUndoToast(entry) {
    if (entry !== undoToastEntry) return;
    const el = document.getElementById('undo-toast');
    if (el) el.classList.add('hidden');
    if (undoToastTimer) {
        clearTimeout(undoToastTimer);
        undoToastTimer = null;
    }
    undoToastEntry = null;
}

// Pop the most recent undo entry, re-insert its email, and ask the server to
// move it back to the inbox it was archived/trashed from — mirrors desktop's
// performUndo (static/app.js) with one deliberate divergence: on failure
// desktop drops the entry for good, but here it goes back on the stack so a
// retry stays possible (v1 UI just doesn't resurface a toast for it). Only
// touches the list DOM when the list is the visible screen (same gating as
// toggleUnread/toggleFlag) so undoing while viewing a *different* email in
// DETAIL doesn't yank the user back to the list.
async function performUndo() {
    const entry = state.undoStack.pop();
    if (!entry) return;

    hideUndoToast(entry);

    const index = Math.min(entry.index, state.emails.length);
    state.emails.splice(index, 0, entry.email);
    if (state.screen === Screen.LIST) renderEmailList();

    // The toast shows while the original archive/trash may still be in
    // flight; firing the move-back concurrently could complete out of order
    // on the server and leave the email archived despite the undo. The
    // optimistic re-insert above stays immediate, but the move-back waits
    // for the action to settle. If the action FAILED, there is nothing to
    // undo: emailAction's catch has already reverted (its includes() guard
    // leaves our re-inserted copy as the only one) and surfaced the error,
    // so just keep the email in the list and skip the move-back.
    if (entry.settled && !(await entry.settled)) return;

    try {
        await state.api('POST', '/emails/' + encodeURIComponent(entry.email.id) + '/move', {
            mailbox_id: entry.mailboxId,
        });
    } catch (err) {
        // Revert the optimistic re-insert — the email stays removed, same
        // as desktop's performUndo.
        const idx = state.emails.indexOf(entry.email);
        if (idx !== -1) state.emails.splice(idx, 1);
        if (state.screen === Screen.LIST) renderEmailList();
        state.undoStack.push(entry);
        capUndoStack();
        showError('Undo', err);
    }
}

async function emailAction(type, emailId) {
    const index = state.emails.findIndex(e => e.id === emailId);
    if (index === -1) return;
    const email = state.emails[index];

    // Optimistic: remove from the list immediately.
    state.emails.splice(index, 1);
    const undoEntry = pushUndo(type, email, index);
    renderEmailList();

    // Literal per-type paths (rather than interpolating `type` into the
    // URL) so /archive and /trash are grep-able route strings, not just an
    // artifact of string concatenation.
    const path = type === 'archive'
        ? '/emails/' + encodeURIComponent(emailId) + '/archive'
        : '/emails/' + encodeURIComponent(emailId) + '/trash';

    // The undo toast shows before this request settles, so performUndo
    // needs to know when — and how — it finished: it awaits entry.settled
    // (true = the action reached the server, safe to move back; false = it
    // failed and the catch below already reverted, nothing to undo). The
    // .then() handlers are attached before our own await, which guarantees
    // this catch runs to completion before performUndo resumes.
    const actionPromise = state.api('POST', path);
    undoEntry.settled = actionPromise.then(() => true, () => false);

    try {
        await actionPromise;
    } catch (err) {
        // Revert: re-insert at the original index and retract OUR undo entry
        // by identity — with two actions in flight, popping the tail could
        // drop a different action's still-valid entry (the failed one isn't
        // necessarily the most recent push). Entry may also already be gone
        // if the capped stack shifted it out.
        const undoIdx = state.undoStack.indexOf(undoEntry);
        if (undoIdx !== -1) state.undoStack.splice(undoIdx, 1);
        // No-op if a later action already replaced our toast.
        hideUndoToast(undoEntry);
        // A fast Undo tap may have optimistically re-inserted this email
        // already (performUndo runs its insert before awaiting entry.settled)
        // — in that case the list is already correct; don't double-insert.
        if (!state.emails.includes(email)) state.emails.splice(index, 0, email);
        renderEmailList();
        showError(type === 'archive' ? 'Archive' : 'Trash', err);
    }
}

function archiveEmail(emailId) {
    return emailAction('archive', emailId);
}

function trashEmail(emailId) {
    return emailAction('trash', emailId);
}

async function toggleUnread(emailId) {
    const email = state.emails.find(e => e.id === emailId);
    const cached = state.emailCache[emailId];
    if (!email && !cached) return;
    const wasUnread = (email || cached).isUnread;

    // Optimistic: flip immediately everywhere the email is held.
    if (email) email.isUnread = !wasUnread;
    if (cached) cached.isUnread = !wasUnread;
    if (state.screen === Screen.LIST) renderEmailList();
    if (state.screen === Screen.DETAIL && state.currentEmailId === emailId) {
        renderDetailActionBar(email || cached);
    }

    const path = '/emails/' + encodeURIComponent(emailId) + (wasUnread ? '/mark-read' : '/mark-unread');

    try {
        await state.api('POST', path);
    } catch (err) {
        // Revert
        if (email) email.isUnread = wasUnread;
        if (cached) cached.isUnread = wasUnread;
        if (state.screen === Screen.LIST) renderEmailList();
        if (state.screen === Screen.DETAIL && state.currentEmailId === emailId) {
            renderDetailActionBar(email || cached);
        }
        showError('Toggle read status', err);
    }
}

async function toggleFlag(emailId) {
    const email = state.emails.find(e => e.id === emailId);
    const cached = state.emailCache[emailId];
    if (!email && !cached) return;
    const wasFlagged = (email || cached).isFlagged;

    // Optimistic: flip immediately everywhere the email is held.
    if (email) email.isFlagged = !wasFlagged;
    if (cached) cached.isFlagged = !wasFlagged;
    if (state.screen === Screen.LIST) renderEmailList();
    if (state.screen === Screen.DETAIL && state.currentEmailId === emailId) {
        renderDetailActionBar(email || cached);
    }

    try {
        await state.api('POST', '/emails/' + encodeURIComponent(emailId) + '/toggle-flag');
    } catch (err) {
        // Revert
        if (email) email.isFlagged = wasFlagged;
        if (cached) cached.isFlagged = wasFlagged;
        if (state.screen === Screen.LIST) renderEmailList();
        if (state.screen === Screen.DETAIL && state.currentEmailId === emailId) {
            renderDetailActionBar(email || cached);
        }
        showError('Toggle star', err);
    }
}

// ============================================================================
// Navigation — screen state model
// ============================================================================
// setScreen() is the single owner of show/hide: one switch toggles the DOM
// and dispatches the per-screen render. It is history-free, so popstate can
// call it directly. navigateTo() is the forward-navigation entry point that
// owns the history push/replace rule before delegating to setScreen().

function setScreen(screen, params = {}) {
    // Leaving compose (Cancel, send, or browser-back) abandons the draft with
    // no confirmation — the simplest correct popstate behavior. Captured
    // before state.screen is overwritten.
    const leavingCompose = state.screen === Screen.COMPOSE && screen !== Screen.COMPOSE;
    state.screen = screen;
    switch (screen) {
        case Screen.COMPOSE:
            state.currentEmailId = null;
            document.getElementById('email-list-wrap').style.display = 'none';
            document.getElementById('app-header').style.display = 'none';
            document.getElementById('email-detail').style.display = 'none';
            document.getElementById('compose-screen').style.display = 'flex';
            // Fields are already prefilled by the entry point (startCompose/
            // startReply/startForward) before navigateTo — nothing to render.
            // Focus the first field the user still needs to fill: To when it's
            // empty (new message, forward), otherwise the body (reply).
            (composeEl('compose-to').value ? composeEl('compose-body') : composeEl('compose-to')).focus();
            break;
        case Screen.DETAIL:
            state.currentEmailId = params.emailId;
            document.getElementById('email-list-wrap').style.display = 'none';
            document.getElementById('app-header').style.display = 'none';
            document.getElementById('compose-screen').style.display = 'none';
            document.getElementById('email-detail').style.display = 'flex';
            renderScreenDetail(params.emailId);
            break;
        case Screen.LIST:
        default:
            state.currentEmailId = null;
            document.getElementById('email-detail').style.display = 'none';
            document.getElementById('compose-screen').style.display = 'none';
            document.getElementById('email-list-wrap').style.display = '';
            document.getElementById('app-header').style.display = '';
            document.getElementById('email-list-wrap').scrollTop = state.listScrollTop;
            renderEmailList();
            break;
    }
    if (leavingCompose) clearComposeFields();
}

// Forward navigation owns history in one place: pushState on list→detail
// (saving the list scroll first), replaceState on detail→detail so paging
// between emails doesn't grow history unbounded. popstate never comes here —
// it applies history's own state via setScreen().
function navigateTo(screen, params = {}) {
    if (screen === Screen.DETAIL && state.screen === Screen.DETAIL) {
        history.replaceState({ screen, ...params }, '');
    } else {
        if (state.screen === Screen.LIST) {
            state.listScrollTop = document.getElementById('email-list-wrap').scrollTop;
        }
        history.pushState({ screen, ...params }, '');
    }
    setScreen(screen, params);
}

// ============================================================================
// Email detail view
// ============================================================================

async function renderScreenDetail(emailId) {
    // Render partial detail from list data immediately
    const listEmail = state.emails.find(e => e.id === emailId);
    if (listEmail) renderEmailDetailPartial(listEmail);

    // Full body: use cache or fetch (delete+reinsert to promote in FIFO-with-promotion cache)
    let full = state.emailCache[emailId];
    const cacheHit = !!full;
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
        // The user may have moved on to a different email while the GET
        // above was in flight. It's still cached above for a later open,
        // but rendering it now or mutating the LIST row's flags would
        // clobber whatever the user is actually looking at.
        if (state.currentEmailId !== emailId) return;
    }

    // Cache-hit opens skip the network GET entirely — prefetchAdjacentEmails
    // fetches with mark_read=false so background warm-up never silently
    // consumes unread state for emails the user hasn't opened. That means
    // the server was never told THIS email is now read; unlike the
    // network-fetch path above (whose GET auto-marks read server-side), we
    // have to ask explicitly. Already-read cache entries (isUnread already
    // false, e.g. reopening something already viewed) don't fire this.
    // Optimistic, matching toggleUnread: flip everywhere the email is held,
    // revert alongside showError on failure — otherwise a failed POST would
    // leave the row rendered read while the server still has it unread.
    if (cacheHit && full.isUnread) {
        full.isUnread = false;
        if (listEmail) listEmail.isUnread = false;
        try {
            await state.api('POST', '/emails/' + encodeURIComponent(emailId) + '/mark-read');
        } catch (err) {
            full.isUnread = true;
            if (listEmail) listEmail.isUnread = true;
            showError('Mark read', err);
        }
        if (state.currentEmailId !== emailId) return;
    }

    renderEmailDetail(full);

    // Network path only: the server auto-marked read on the GET above, so
    // mirror it locally. (The cache-hit path reconciled its own flags around
    // the explicit mark-read POST — flattening them here would clobber its
    // failure revert.)
    if (!cacheHit) {
        if (listEmail?.isUnread) listEmail.isUnread = false;
        if (full.isUnread) full.isUnread = false;
    }
    // renderEmailDetail → renderEmailDetailPartial already drew the action
    // bar from the pre-correction unread flag; redraw with the now-correct
    // (read) state.
    renderDetailActionBar(full);

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
    renderDetailActionBar(email);
}

// Reflects the current email's read/starred state onto the detail action
// bar's read and star buttons. Archive/trash are stateless (always the same
// icon) so they need no equivalent here.
function renderDetailActionBar(email) {
    if (!email) return;
    const readBtn = document.getElementById('detail-read-btn');
    if (readBtn) {
        readBtn.textContent = email.isUnread ? '●' : '○';
        readBtn.setAttribute('aria-label', email.isUnread ? 'Mark as read' : 'Mark as unread');
        readBtn.setAttribute('aria-pressed', String(!!email.isUnread));
    }
    const starBtn = document.getElementById('detail-star-btn');
    if (starBtn) {
        starBtn.textContent = email.isFlagged ? '★' : '☆';
        starBtn.classList.toggle('active', !!email.isFlagged);
        starBtn.setAttribute('aria-label', email.isFlagged ? 'Remove star' : 'Add star');
        starBtn.setAttribute('aria-pressed', String(!!email.isFlagged));
    }
}

// Archive/trash from the detail view auto-advance: stay in DETAIL on the
// next email in the list (navigateTo replaces history since we're already
// on DETAIL), or fall back to LIST via history.back() — mirroring the
// back-btn handler below — when there's nothing after it. The action itself
// is optimistic (emailAction), so we advance immediately rather than
// waiting on the network round-trip; a failure reverts state.emails and
// toasts in the background without pulling the user back.
function handleDetailAction(type) {
    const emailId = state.currentEmailId;
    if (!emailId) return;
    const index = state.emails.findIndex(e => e.id === emailId);
    const next = index !== -1 ? state.emails[index + 1] : null;

    if (type === 'archive') archiveEmail(emailId);
    else trashEmail(emailId);

    if (next) {
        navigateTo(Screen.DETAIL, { emailId: next.id });
    } else {
        history.back();
    }
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
    // see, so log instead of toasting. mark_read=false: prefetching must
    // not silently consume unread state for emails the user never opened.
    Promise.all(toFetch.map(id =>
        state.api('GET', '/emails/' + encodeURIComponent(id) + '?mark_read=false').then(e => cacheEmail(e))
    )).catch(err => console.warn('Prefetch failed:', err));
}

// ============================================================================
// Compose — new / reply / reply-all / forward
// ============================================================================
// Mirrors desktop's compose semantics (static/app.js sendEmail/startReply/
// startForward/autoSelectFromAddress): the four entry points prefill the DOM
// fields and stash a replyContext (quote + threading id), then navigate to
// Screen.COMPOSE. The quote is never edited — it's appended at send time
// exactly like desktop, both as '> '-prefixed plain text and as an HTML
// <blockquote>. showError is the only failure sink.

const COMPOSE_EL = {};

function composeEl(id) {
    return (COMPOSE_EL[id] ||= document.getElementById(id));
}

// Identities power the From selector. Account-scoped: the passed acct is
// captured so a response landing after an account switch is discarded. A
// failure just leaves the selector empty (hidden) — compose still works with
// the account's default From, so this reports and moves on.
async function loadIdentities(acct) {
    try {
        const identities = await state.api('GET', '/identities');
        if (state.currentAccount?.id !== acct) return;
        state.identities = Array.isArray(identities) ? identities : [];
        state.identitiesAccount = acct;
    } catch (err) {
        if (state.currentAccount?.id !== acct) return;
        state.identities = [];
        state.identitiesAccount = acct;
        showError('Load identities', err);
    }
    renderComposeFrom();
}

// Populate the From <select> and hide the whole row when there's 0 or 1
// identity (nothing to choose). The single option still carries its value so
// sendComposedEmail reads a from_address even while the row is hidden.
function renderComposeFrom() {
    const sel = composeEl('compose-from');
    const row = composeEl('compose-from-row');
    if (!sel || !row) return;
    sel.innerHTML = state.identities.map(id =>
        '<option value="' + escapeHtml(id.email) + '">'
        + escapeHtml(id.email) + (id.name ? ' (' + escapeHtml(id.name) + ')' : '')
        + '</option>'
    ).join('');
    row.classList.toggle('hidden', state.identities.length <= 1);
}

// Match an identity to the original email's recipients — To first, then Cc —
// so a reply goes out from the address it was sent to. Falls back to the
// select's default (first option) when nothing matches. Mirrors desktop.
function autoSelectFromAddress(email) {
    const sel = composeEl('compose-from');
    if (!sel || !state.identities.length) return;
    const lists = [email.to || [], email.cc || []];
    for (const list of lists) {
        for (const r of list) {
            if (!r.email) continue;
            const addr = r.email.toLowerCase();
            for (const id of state.identities) {
                if (id.email.toLowerCase() === addr) {
                    sel.value = id.email;
                    return;
                }
            }
        }
    }
}

function htmlToPlainText(html) {
    const doc = new DOMParser().parseFromString(html, 'text/html');
    return doc.body.innerText || '';
}

// The full email (with body + recipients) backing a reply/forward. The detail
// view guarantees it's cached before its action bar is reachable.
function getComposeEmail() {
    return state.emailCache[state.currentEmailId] || null;
}

// Reset every field to empty and drop any reply/forward context. Field-only:
// the screen show/hide stays in setScreen.
function clearComposeFields() {
    composeEl('compose-to').value = '';
    composeEl('compose-cc').value = '';
    composeEl('compose-subject').value = '';
    composeEl('compose-body').value = '';
    autosizeComposeBody();
    const quote = composeEl('compose-quote');
    quote.classList.add('hidden');
    composeEl('compose-quote-body').replaceChildren();
    composeEl('compose-quote-body').classList.add('hidden');
    composeEl('compose-quote-toggle').textContent = 'Show quoted text';
    hideDiscardBar();
    state.replyContext = null;
    if (state.identities.length) composeEl('compose-from').value = state.identities[0].email;
}

// Renders the read-only quote preview: a header line plus the original body
// (HTML in a sandboxed iframe, else plain text in a <pre>). Collapsed by
// default — the toggle button reveals it. Content is display only; send-time
// quoting reads state.replyContext, not this DOM.
function renderComposeQuote(headerHtml, quotedHtml, quotedText) {
    const body = composeEl('compose-quote-body');
    body.replaceChildren();
    const header = document.createElement('div');
    header.className = 'quote-header';
    header.innerHTML = headerHtml;
    body.appendChild(header);
    if (quotedHtml) {
        const host = document.createElement('div');
        body.appendChild(host);
        renderHtmlBodyIframe(host, quotedHtml);
    } else {
        const pre = document.createElement('pre');
        pre.textContent = quotedText;
        body.appendChild(pre);
    }
    body.classList.add('hidden');
    composeEl('compose-quote-toggle').textContent = 'Show quoted text';
    composeEl('compose-quote').classList.remove('hidden');
}

function toggleComposeQuote() {
    const body = composeEl('compose-quote-body');
    const collapsed = body.classList.toggle('hidden');
    composeEl('compose-quote-toggle').textContent = collapsed ? 'Show quoted text' : 'Hide quoted text';
}

function setComposeTitle(text) {
    composeEl('compose-title').textContent = text;
}

// New blank message.
function startCompose() {
    state.replyContext = null;
    clearComposeFields();
    setComposeTitle('New message');
    navigateTo(Screen.COMPOSE);
}

// Reply / reply-all. Mirrors desktop startReply: To = original sender,
// subject Re:… (no double-prefix), quote = htmlBody (else textBody),
// in_reply_to = email.id. Reply-all additionally Cc's the original To
// recipients (desktop's exact rule — email.to only, filtered of blanks).
function startReply(replyAll) {
    const email = getComposeEmail();
    if (!email) return;

    clearComposeFields();

    const from = email.from?.[0];
    composeEl('compose-to').value = from?.email || '';

    if (replyAll && email.to) {
        composeEl('compose-cc').value = email.to
            .filter(t => t.email)
            .map(t => t.email)
            .join(', ');
    }

    composeEl('compose-subject').value = email.subject.startsWith('Re:')
        ? email.subject
        : 'Re: ' + email.subject;

    const quotedHtml = email.htmlBody || null;
    const quotedText = email.htmlBody ? htmlToPlainText(email.htmlBody) : (email.textBody || '');

    state.replyContext = { inReplyTo: email.id, quotedHtml, quotedText };

    autoSelectFromAddress(email);

    const header = 'On ' + formatDate(email.receivedAt) + ', '
        + escapeHtml(from?.name || from?.email || '') + ' wrote:';
    renderComposeQuote(header, quotedHtml, quotedText);

    setComposeTitle(replyAll ? 'Reply all' : 'Reply');
    navigateTo(Screen.COMPOSE);
}

// Forward. Mirrors desktop startForward: subject Fwd:…, forwarded-message
// header + quote, and NO in_reply_to (forwarding starts a new thread).
// Desktop leaves To and Cc empty on a forward — the user picks recipients —
// so this does too.
function startForward() {
    const email = getComposeEmail();
    if (!email) return;

    clearComposeFields();

    composeEl('compose-subject').value = email.subject.startsWith('Fwd:')
        ? email.subject
        : 'Fwd: ' + email.subject;

    const from = email.from?.[0];
    const quotedHtml = email.htmlBody || null;
    const quotedText = email.htmlBody ? htmlToPlainText(email.htmlBody) : (email.textBody || '');

    // No inReplyTo → send maps it to null: a forward is a fresh thread.
    state.replyContext = { quotedHtml, quotedText };

    autoSelectFromAddress(email);

    const header = '---------- Forwarded message ---------<br>'
        + 'From: ' + escapeHtml(from?.name || '') + ' &lt;' + escapeHtml(from?.email || '') + '&gt;<br>'
        + 'Subject: ' + escapeHtml(email.subject);
    renderComposeQuote(header, quotedHtml, quotedText);

    setComposeTitle('Forward');
    navigateTo(Screen.COMPOSE);
}

// Auto-expanding body textarea: grow to fit content so the compose column
// scrolls as one, no inner textarea scrollbar.
function autosizeComposeBody() {
    const ta = composeEl('compose-body');
    if (!ta) return;
    ta.style.height = 'auto';
    ta.style.height = ta.scrollHeight + 'px';
}

// Toggles the Send button between idle and in-flight. The lock lives on
// state.sending; the button's disabled state mirrors it for the user.
function setComposeSending(sending) {
    state.sending = sending;
    const btn = composeEl('compose-send-btn');
    if (!btn) return;
    btn.disabled = sending;
    btn.textContent = sending ? 'Sending…' : 'Send';
}

// Send. Payload mirrors desktop sendEmail exactly: plain body carries the
// '> '-quoted original, html_body (only when quoting HTML) wraps the user's
// text in a <div> above a <blockquote> of the original. Validates ≥1 To,
// locks against double-send, reports failures via showError with the form
// left intact for a retry.
async function sendComposedEmail() {
    if (state.sending) return;

    const to = composeEl('compose-to').value.split(',').map(s => s.trim()).filter(Boolean);
    const cc = composeEl('compose-cc').value.split(',').map(s => s.trim()).filter(Boolean);
    const fromAddress = composeEl('compose-from').value || null;
    const subject = composeEl('compose-subject').value;
    const userText = composeEl('compose-body').value;

    if (!to.length) {
        showError('Send', new Error('add at least one recipient'));
        return;
    }

    const quotedText = state.replyContext?.quotedText;
    const quotedHtml = state.replyContext?.quotedHtml;

    const fullTextBody = quotedText
        ? userText + '\n\n' + quotedText.split('\n').map(l => '> ' + l).join('\n')
        : userText;

    const fullHtmlBody = quotedHtml
        ? '<div>' + escapeHtml(userText).replace(/\n/g, '<br>') + '</div>'
          + '<blockquote style="border-left:2px solid #ccc;padding-left:12px;margin-left:0">' + quotedHtml + '</blockquote>'
        : null;

    setComposeSending(true);
    try {
        await state.api('POST', '/emails/send', {
            to,
            cc,
            subject,
            body: fullTextBody,
            html_body: fullHtmlBody || undefined,
            in_reply_to: state.replyContext?.inReplyTo || null,
            from_address: fromAddress,
        });
        // The user may have browser-backed out of compose while the send was
        // in flight — setScreen already popped the history entry on the way
        // out, so firing history.back() now would pop a SECOND entry
        // (detail→list, or clean out of the app), and a "Sent" toast for a
        // draft they abandoned would just confuse. The send itself succeeded;
        // only make sure no draft state lingers.
        if (state.screen === Screen.COMPOSE) {
            showToast('Sent', 3000);
            clearComposeFields();
            history.back();
        } else {
            clearComposeFields();
        }
    } catch (err) {
        // Always surface the failure — even if the user already left compose,
        // a silently dropped send would look like it went out. But only the
        // still-on-compose case keeps the form for a retry; never touch
        // history from a stale completion.
        showError('Send', err);
    } finally {
        setComposeSending(false);
    }
}

// Cancel: discard immediately when the draft is empty, else surface the
// inline "Discard draft?" bar (no blocking confirm() — it would freeze the
// automation harness).
function cancelCompose() {
    const dirty = composeEl('compose-subject').value.trim() || composeEl('compose-body').value.trim();
    if (dirty) {
        showDiscardBar();
    } else {
        discardCompose();
    }
}

function discardCompose() {
    clearComposeFields();
    history.back();
}

function showDiscardBar() {
    composeEl('compose-discard-bar').classList.remove('hidden');
}

function hideDiscardBar() {
    composeEl('compose-discard-bar').classList.add('hidden');
}

// ============================================================================
// Gesture controller
// ============================================================================
// One controller owns touchstart/touchmove/touchend on the list wrap. Each
// gesture is claimed by exactly one recognizer, and the controller is the
// ONLY place that calls preventDefault (so touchmove stays non-passive).
// A single eligible recognizer locks at touchstart — e.g. pull-to-refresh at
// the top of the list, or a row-swipe when scrolled down (the only
// recognizer whose canStart() matches a mid-list touch). When more than one
// recognizer is eligible — a row touched at scrollTop 0, where both
// pull-to-refresh and row-swipe canStart() — the choice is deferred to the
// first move and made by drag axis. Adding a recognizer is a push onto
// `recognizers` — never another addEventListener set.

// Pull-to-refresh recognizer: a downward drag from the top of the list.
const pullToRefreshRecognizer = {
    axis: 'y',
    startY: 0,
    // Eligible only when the list is scrolled to the very top.
    canStart(ctx) {
        return ctx.listWrap.scrollTop === 0;
    },
    start(ctx) {
        this.startY = ctx.startY;
    },
    // Returns true to preventDefault — we consume every downward move.
    move(ctx) {
        const dy = ctx.y - this.startY;
        if (dy <= 0) return false;
        const indicator = document.getElementById('pull-indicator');
        if (dy < 120) {
            indicator.style.height = dy + 'px';
            indicator.style.opacity = Math.min(dy / 60, 1);
        }
        return true;
    },
    end() {
        const indicator = document.getElementById('pull-indicator');
        const h = parseInt(indicator.style.height) || 0;
        if (h > 60) {
            indicator.style.height = '40px';
            indicator.textContent = 'Refreshing...';
            loadEmails();
        } else {
            finishPullRefresh();
        }
    },
};

// Row-swipe recognizer: horizontal drag on a `.email-row` — right reveals
// archive (green), left reveals trash (red); crossing the threshold on
// release performs the action, otherwise the row springs back (CSS
// transition, toggled off via `.swiping` while actively dragging so the
// translate tracks the finger 1:1).
//
// canStart() matches ANY touch on a row, so below the top of the list
// (where pull-to-refresh isn't a candidate) this is the sole eligible
// recognizer and locks immediately, before we know the drag direction —
// unlike pull-to-refresh's own scrollTop gate, canStart() here can't see
// direction yet. move() self-gates on axis the same way pull-to-refresh
// self-gates on dy<=0: if the drag turns out to be vertical, it declines
// (no preventDefault) so the list keeps scrolling normally.
const SWIPE_TRIGGER_MIN_PX = 80;
const SWIPE_TRIGGER_RATIO = 0.4; // ~40% of row width

const rowSwipeRecognizer = {
    axis: 'x',
    row: null,
    width: 0,
    dx: 0,
    canStart(ctx) {
        return !!ctx.target && !!ctx.target.closest('.email-row');
    },
    start(ctx) {
        this.row = ctx.target.closest('.email-row');
        this.width = this.row.offsetWidth;
        this.dx = 0;
        this.row.classList.add('swiping');
    },
    // Returns true to preventDefault — once we've committed to a horizontal
    // drag we own it so the list doesn't also try to scroll under it.
    move(ctx) {
        if (!this.row) return false;
        const dx = ctx.x - ctx.startX;
        const dy = ctx.y - ctx.startY;
        if (Math.abs(dy) > Math.abs(dx)) return false;
        this.dx = dx;
        this.row.style.transform = 'translateX(' + dx + 'px)';
        const bg = this.row.parentElement.querySelector('.swipe-bg');
        if (bg) {
            bg.classList.toggle('swipe-reveal-archive', dx > 0);
            bg.classList.toggle('swipe-reveal-trash', dx < 0);
        }
        return true;
    },
    end() {
        const row = this.row;
        if (!row) return;
        const dx = this.dx;
        const id = row.dataset.id;
        const triggered = Math.abs(dx) > SWIPE_TRIGGER_MIN_PX
            || Math.abs(dx) > this.width * SWIPE_TRIGGER_RATIO;

        row.classList.remove('swiping');
        row.style.transform = '';
        const bg = row.parentElement.querySelector('.swipe-bg');
        if (bg) bg.classList.remove('swipe-reveal-archive', 'swipe-reveal-trash');
        this.row = null;

        if (!triggered || !id) return;
        if (dx > 0) archiveEmail(id);
        else trashEmail(id);
    },
};

const gestureController = {
    listWrap: null,
    recognizers: [],
    candidates: [],
    active: null,
    startX: 0,
    startY: 0,

    init() {
        this.listWrap = document.getElementById('email-list-wrap');
        if (!this.listWrap) return;
        this.recognizers = [pullToRefreshRecognizer, rowSwipeRecognizer];
        this.listWrap.addEventListener('touchstart', (e) => this.onStart(e), { passive: true });
        this.listWrap.addEventListener('touchmove', (e) => this.onMove(e), { passive: false });
        this.listWrap.addEventListener('touchend', () => this.onEnd());
    },

    ctx(touch) {
        return {
            listWrap: this.listWrap,
            x: touch.clientX,
            y: touch.clientY,
            startX: this.startX,
            startY: this.startY,
            // A Touch's `target` is the element it started on, even once
            // touchmove carries it elsewhere — lets rowSwipeRecognizer find
            // its `.email-row` from the touchstart ctx alone.
            target: touch.target,
        };
    },

    onStart(e) {
        const t = e.touches[0];
        this.startX = t.clientX;
        this.startY = t.clientY;
        const ctx = this.ctx(t);
        this.candidates = this.recognizers.filter(r => r.canStart(ctx));
        // Lock immediately when only one recognizer is eligible (today's path,
        // identical to the old behavior); otherwise defer to onMove's axis pick.
        this.active = this.candidates.length === 1 ? this.candidates[0] : null;
        if (this.active) this.active.start(ctx);
    },

    onMove(e) {
        if (!this.candidates.length) return;
        const ctx = this.ctx(e.touches[0]);
        if (!this.active) {
            const dx = ctx.x - this.startX;
            const dy = ctx.y - this.startY;
            const axis = Math.abs(dx) > Math.abs(dy) ? 'x' : 'y';
            this.active = this.candidates.find(r => r.axis === axis) || null;
            if (!this.active) {
                this.candidates = [];
                return;
            }
            this.active.start(ctx);
        }
        if (this.active.move(ctx)) e.preventDefault();
    },

    onEnd() {
        if (this.active) this.active.end();
        this.active = null;
        this.candidates = [];
    },
};

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
    if (id) navigateTo(Screen.DETAIL, { emailId: id });
});

// Detail action bar: archive/trash auto-advance (handleDetailAction); read
// and star toggle the current email in place.
document.getElementById('detail-archive-btn').addEventListener('click', () => handleDetailAction('archive'));
document.getElementById('detail-trash-btn').addEventListener('click', () => handleDetailAction('trash'));
document.getElementById('detail-read-btn').addEventListener('click', () => {
    if (state.currentEmailId) toggleUnread(state.currentEmailId);
});
document.getElementById('detail-star-btn').addEventListener('click', () => {
    if (state.currentEmailId) toggleFlag(state.currentEmailId);
});

// Compose entry points: header ✎ (new message) and the detail action bar's
// reply / reply-all / forward.
document.getElementById('compose-btn').addEventListener('click', startCompose);
document.getElementById('detail-reply-btn').addEventListener('click', () => startReply(false));
document.getElementById('detail-reply-all-btn').addEventListener('click', () => startReply(true));
document.getElementById('detail-forward-btn').addEventListener('click', startForward);

// Compose screen controls.
document.getElementById('compose-cancel-btn').addEventListener('click', cancelCompose);
document.getElementById('compose-send-btn').addEventListener('click', sendComposedEmail);
document.getElementById('compose-discard-keep').addEventListener('click', hideDiscardBar);
document.getElementById('compose-discard-confirm').addEventListener('click', discardCompose);
document.getElementById('compose-quote-toggle').addEventListener('click', toggleComposeQuote);
document.getElementById('compose-body').addEventListener('input', autosizeComposeBody);
// Keyboard-aware without visualViewport gymnastics: nudge the focused field
// into view above the on-screen keyboard (100dvh + native scroll do the rest).
document.getElementById('compose-fields').addEventListener('focusin', (e) => {
    if (e.target.matches('input, textarea, select')) {
        e.target.scrollIntoView({ block: 'nearest' });
    }
});

// Undo toast — tap anywhere on it to undo (no keyboard shortcut on a phone).
document.getElementById('undo-toast').addEventListener('click', performUndo);

// Browser back/forward — history is the source of truth for the current
// screen; apply whatever the entry carries (defaulting to the list). No
// forward-nav guessing.
window.addEventListener('popstate', (e) => {
    setScreen(e.state?.screen ?? Screen.LIST, e.state ?? {});
});

// Replace initial history state
history.replaceState({ screen: Screen.LIST }, '');

// Register service worker. Skipped outside a secure context (plain http
// on anything but localhost): on Chromium the serviceWorker API still
// exists there but register() rejects, while on Firefox it isn't exposed
// at all — either way, checking isSecureContext first avoids depending on
// the exact per-browser failure mode.
if ('serviceWorker' in navigator) {
    if (window.isSecureContext) {
        navigator.serviceWorker.register('/mobile/sw.js', { scope: '/mobile/' })
            .catch((err) => console.warn('Service worker registration failed:', err));
    } else {
        console.info('Skipping service worker registration: not a secure context');
    }
}

gestureController.init();
setupInfiniteScroll();
init();

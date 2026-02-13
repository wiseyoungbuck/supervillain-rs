// Supervillain - The open anti-superhuman email client
// Direct, readable code. No framework, no build step.

const state = {
    mode: 'normal',           // normal, insert, command, search
    view: 'list',             // list, detail, compose
    accounts: [],
    currentAccount: null,
    mailboxes: [],
    currentMailbox: null,
    emails: [],
    selectedIndex: 0,
    currentEmail: null,
    searchQuery: '',
    undoStack: [],
    pendingG: false,          // for gg command
    commandPaletteIndex: 0,
    replyContext: null,       // for reply/forward
    identities: [],           // send-as email addresses
    splits: [],               // split inbox definitions
    currentSplit: 'all',      // currently active split tab
};

// Simple cache: email id -> full email object with body
const emailCache = {};
// Scroll position cache: email id -> scrollTop
const scrollPositions = {};

// Rolling email cache
const CACHE_LIMIT = 150;
const REFILL_THRESHOLD = 100;
let refillInFlight = false;

// DOM elements
const els = {};

function init() {
    // Cache DOM elements
    els.modeIndicator = document.getElementById('mode-indicator');
    els.mailboxName = document.getElementById('mailbox-name');
    els.emailCount = document.getElementById('email-count');
    els.statusMessage = document.getElementById('status-message');
    els.accountSelector = document.getElementById('account-selector');
    els.mailboxList = document.getElementById('mailbox-list');
    els.emailList = document.getElementById('email-list');
    els.emailListView = document.getElementById('email-list-view');
    els.emailDetailView = document.getElementById('email-detail-view');
    els.emailSubject = document.getElementById('email-subject');
    els.emailMeta = document.getElementById('email-meta');
    els.emailBody = document.getElementById('email-body');
    els.composeView = document.getElementById('compose-view');
    els.composeFrom = document.getElementById('compose-from');
    els.composeTo = document.getElementById('compose-to');
    els.composeCc = document.getElementById('compose-cc');
    els.composeSubject = document.getElementById('compose-subject');
    els.composeBody = document.getElementById('compose-body');
    els.commandPalette = document.getElementById('command-palette');
    els.commandInput = document.getElementById('command-input');
    els.commandResults = document.getElementById('command-results');
    els.searchBar = document.getElementById('search-bar');
    els.searchInput = document.getElementById('search-input');
    els.helpOverlay = document.getElementById('help-overlay');
    els.undoToast = document.getElementById('undo-toast');
    els.undoMessage = document.getElementById('undo-message');
    els.undoButton = document.getElementById('undo-button');
    els.splitTabs = document.getElementById('split-tabs');
    els.splitModal = document.getElementById('split-modal');
    els.splitName = document.getElementById('split-name');
    els.splitFilterType = document.getElementById('split-filter-type');
    els.splitPattern = document.getElementById('split-pattern');
    els.splitCancel = document.getElementById('split-cancel');
    els.splitSave = document.getElementById('split-save');
    els.splitPatternField = document.getElementById('split-pattern-field');
    els.splitHint = document.getElementById('split-hint');
    els.calendarEvent = document.getElementById('calendar-event');
    els.calTitle = document.getElementById('cal-title');
    els.calDatetime = document.getElementById('cal-datetime');
    els.calLocation = document.getElementById('cal-location');
    els.calAttendees = document.getElementById('cal-attendees');
    els.rsvpAccept = document.getElementById('rsvp-accept');
    els.rsvpMaybe = document.getElementById('rsvp-maybe');
    els.rsvpDecline = document.getElementById('rsvp-decline');
    els.calAdd = document.getElementById('cal-add');

    // Event listeners
    document.addEventListener('keydown', handleKeyDown);
    els.commandInput.addEventListener('input', handleCommandInput);
    els.searchInput.addEventListener('keydown', handleSearchKeyDown);
    els.undoButton.addEventListener('click', performUndo);
    els.splitCancel.addEventListener('click', closeSplitModal);
    els.splitSave.addEventListener('click', saveSplit);
    els.splitFilterType.addEventListener('change', updateSplitModalFields);
    els.rsvpAccept.addEventListener('click', () => rsvpToEvent('ACCEPTED'));
    els.rsvpMaybe.addEventListener('click', () => rsvpToEvent('TENTATIVE'));
    els.rsvpDecline.addEventListener('click', () => rsvpToEvent('DECLINED'));
    els.calAdd.addEventListener('click', addToCalendar);

    // Compose field listeners
    [els.composeTo, els.composeCc, els.composeSubject, els.composeBody].forEach(el => {
        el.addEventListener('focus', () => setMode('insert'));
        el.addEventListener('blur', () => setMode('normal'));
    });

    // Reload theme on window focus (pick up theme changes after alt-tabbing back)
    window.addEventListener('focus', loadTheme);

    // Load data
    loadTheme();
    loadAccounts();
    loadSplits();
}

// Theme

async function loadTheme() {
    try {
        const css = await fetch('/api/theme').then(r => r.text());
        let el = document.getElementById('omarchy-theme');
        if (!el) {
            el = document.createElement('style');
            el.id = 'omarchy-theme';
            document.head.appendChild(el);
        }
        el.textContent = css;

        // In light mode, don't force dark colors on HTML email content
        const isLight = css.includes('--light-mode');
        document.body.classList.toggle('light-theme', isLight);
    } catch (err) {
        console.warn('Failed to load theme:', err);
    }
}

// API calls

async function api(method, path, body = null) {
    const opts = {
        method,
        headers: { 'Content-Type': 'application/json' },
    };
    if (body) opts.body = JSON.stringify(body);

    // Add account parameter if we have a current account
    let url = '/api' + path;
    if (state.currentAccount) {
        const separator = url.includes('?') ? '&' : '?';
        url += `${separator}account=${state.currentAccount.id}`;
    }

    const resp = await fetch(url, opts);
    if (!resp.ok) {
        const err = await resp.text();
        throw new Error(err);
    }
    return resp.json();
}

async function loadAccounts() {
    try {
        state.accounts = await fetch('/api/accounts').then(r => r.json());
        renderAccounts();

        // Select default account
        const defaultAcc = state.accounts.find(a => a.isDefault) || state.accounts[0];
        if (defaultAcc) selectAccount(defaultAcc);
    } catch (err) {
        showStatus('Failed to load accounts: ' + err.message, 'error');
    }
}

function renderAccounts() {
    if (state.accounts.length <= 1) {
        els.accountSelector.style.display = 'none';
        return;
    }

    els.accountSelector.style.display = 'block';
    els.accountSelector.innerHTML = state.accounts.map((acc, idx) => `
        <div class="account-item ${state.currentAccount?.id === acc.id ? 'active' : ''}"
             data-id="${acc.id}">
            <span class="account-key">${idx + 1}</span>
            <span class="account-email">${acc.email}</span>
            <span class="account-provider">${acc.provider}</span>
        </div>
    `).join('');

    els.accountSelector.querySelectorAll('.account-item').forEach(el => {
        el.addEventListener('click', () => {
            const acc = state.accounts.find(a => a.id === el.dataset.id);
            if (acc) selectAccount(acc);
        });
    });
}

function selectAccount(account) {
    state.currentAccount = account;
    state.mailboxes = [];
    state.emails = [];
    state.currentMailbox = null;
    state.selectedIndex = 0;
    state.currentSplit = 'all';
    renderAccounts();
    loadMailboxes();
    loadIdentities();
}

async function loadSplits() {
    try {
        state.splits = await fetch('/api/splits').then(r => r.json());
        renderSplitTabs();
    } catch (err) {
        console.warn('Failed to load splits:', err);
        state.splits = [];
    }
}

async function loadIdentities() {
    try {
        state.identities = await api('GET', '/identities');
        renderFromDropdown();
    } catch (err) {
        console.warn('Failed to load identities:', err);
        state.identities = [];
    }
}

function renderFromDropdown() {
    if (!els.composeFrom) return;
    els.composeFrom.innerHTML = state.identities.map(id =>
        `<option value="${id.email}">${id.name ? id.name + ' <' + id.email + '>' : id.email}</option>`
    ).join('');
}

// Provider icons from dashboardicons.com
const SPLIT_ICONS = {
    aristotle: `<svg class="split-icon" width="14" height="14" viewBox="0 17.9 512.1 476.2"><path d="M512 267.9c0-4-2-7.7-5.5-9.8h-.1l-.2-.1-177.4-105c-.8-.5-1.6-1-2.4-1.4-6.9-3.5-15-3.5-21.8 0-.8.4-1.6.9-2.4 1.4L124.8 258l-.2.1c-5.4 3.4-7.1 10.5-3.7 15.9 1 1.6 2.4 2.9 4 3.9l177.4 105c.8.5 1.6 1 2.4 1.4 6.9 3.5 15 3.5 21.8 0 .8-.4 1.6-.9 2.4-1.4l177.4-105c3.6-2.1 5.7-5.9 5.7-10" fill="#0a2767"/><path d="M145.5 197.8H262v106.7H145.5zM488.2 89.3V40.5c.3-12.2-9.4-22.3-21.6-22.6H164.5c-12.2.3-21.9 10.4-21.6 22.6v48.8l178.6 47.6z" fill="#0364b8"/><path d="M142.9 89.3H262v107.2H142.9z" fill="#0078d4"/><path d="M381 89.3H262v107.2l119 107.1h107.2V196.5z" fill="#28a8ea"/><path d="M262 196.5h119v107.2H262z" fill="#0078d4"/><path d="M262 303.6h119v107.2H262z" fill="#0364b8"/><path d="M145.5 304.5H262v97H145.5z" fill="#14447d"/><path d="M381 303.6h107.2v107.2H381z" fill="#0078d4"/><path d="m506.5 277.2-.2.1-177.4 99.8c-.8.5-1.6.9-2.4 1.3-3 1.4-6.3 2.2-9.6 2.4l-9.7-5.7c-.8-.4-1.6-.9-2.4-1.4L125 271.2l-5.9-3.3v202c.1 13.5 11.1 24.3 24.6 24.2h344.2c.2 0 .4-.1.6-.1 2.8-.2 5.7-.8 8.3-1.7 1.2-.5 2.3-1.1 3.3-1.7.8-.5 2.2-1.4 2.2-1.4 6.1-4.5 9.7-11.6 9.7-19.2V268c0 3.8-2.1 7.3-5.5 9.2" fill="#28a8ea"/><path d="M262 146.8V377c0 12-9.7 21.8-21.8 21.9H119.1V125h121.1c12 0 21.8 9.8 21.8 21.8" fill="rgba(0,0,0,.1)"/><path d="M21.8 125h218.3c12.1 0 21.8 9.8 21.8 21.8v218.3c0 12.1-9.8 21.8-21.8 21.8H21.8C9.8 387 0 377.2 0 365.2V146.8c0-12 9.8-21.8 21.8-21.8" fill="#0078d4"/><path d="M68.2 216.6c5.4-11.5 14.1-21.1 24.9-27.5 12-6.9 25.7-10.3 39.6-9.9 12.9-.3 25.5 3 36.7 9.4 10.5 6.2 18.9 15.4 24.3 26.3 5.8 12 8.8 25.3 8.5 38.7.3 14-2.7 27.9-8.8 40.5-5.5 11.3-14.2 20.8-25 27.2-11.6 6.6-24.7 10-38 9.7-13.1.3-26.1-3-37.5-9.5-10.5-6.4-19.1-15.5-24.6-26.5-5.9-11.9-8.8-25-8.6-38.2-.2-13.9 2.7-27.6 8.5-40.2m26.6 64.6c2.9 7.2 7.7 13.5 14 18.1 6.4 4.5 14.1 6.8 21.9 6.6 8.3.3 16.5-2.1 23.4-6.8 6.2-4.6 11-10.9 13.6-18.1 3-8.1 4.5-16.7 4.3-25.3.1-8.7-1.3-17.4-4.1-25.6-2.5-7.4-7.1-14-13.2-18.9-6.7-5-14.9-7.5-23.2-7.1-8-.2-15.8 2.1-22.4 6.7-6.4 4.6-11.4 11-14.3 18.3-6.4 16.7-6.4 35.3 0 52.1" fill="#fff"/><path d="M381 89.3h107.2v107.2H381z" fill="#50d9ff"/></svg>`,
    aristoi: `<svg class="split-icon" width="14" height="14" viewBox="0 0 512 512"><path d="M53.8 256c0-111.7 90.5-202.2 202.2-202.2 70.2 0 132 35.7 168.2 90l39.7 7.8 5-37.7C423 45.3 344.8 0 256 0 114.7 0 0 114.7 0 256c0 52.6 15.8 101.3 43 142.1l38.5 5 6.3-35c-21.4-32.1-34-70.6-34-112.1" fill="#0067b9"/><path d="M469.5 114.9c-.3-.3-.5-.5-.8-1L424 143.8c.3.3.5.5.8 1 21.1 31.9 33.4 70.2 33.4 111.2 0 111.7-90.5 202.2-202.2 202.2-69.7 0-131.3-35.2-167.5-89-.3-.3-.5-.8-.5-1l-44.8 29.9c.3.3.5.8.5 1C89.8 467.2 167.7 512 256 512c141.3 0 256-114.7 256-256 0-52.1-15.6-100.6-42.5-141.1" fill="#69b3e7"/><path d="m256 256-121.7-81.2V337c1.3-.8 2.3-1.5 0 0l76.4-23.6z" fill="#ffc107"/><path d="M134.3 337h234.9c4.8 0 8.6-3.8 8.6-8.6V174.8z" fill="#333e48"/></svg>`,
};

function getSplitIcon(splitId) {
    return SPLIT_ICONS[splitId] || '';
}

function renderSplitTabs() {
    // only show tabs when viewing inbox
    const isInbox = state.currentMailbox?.role === 'inbox';
    if (!isInbox || state.splits.length === 0) {
        els.splitTabs.classList.remove('visible');
        return;
    }

    els.splitTabs.classList.add('visible');

    // "All" tab first, then each configured split
    const tabs = [
        { id: 'all', name: 'All' },
        ...state.splits
    ];

    els.splitTabs.innerHTML = tabs.map((split, idx) => `
        <div class="split-tab ${state.currentSplit === split.id ? 'active' : ''}"
             data-split="${split.id}">
            ${getSplitIcon(split.id)}
            ${escapeHtml(split.name)}
            <span class="split-shortcut">^${idx + 1}</span>
        </div>
    `).join('');

    els.splitTabs.querySelectorAll('.split-tab').forEach(el => {
        el.addEventListener('click', () => selectSplit(el.dataset.split));
    });
}

function selectSplit(splitId) {
    state.currentSplit = splitId;
    renderSplitTabs();
    loadEmails();
}

function cycleSplit(direction) {
    if (state.currentMailbox?.role !== 'inbox' || state.splits.length === 0) return;

    const allTabs = ['all', ...state.splits.map(s => s.id)];
    const currentIdx = allTabs.indexOf(state.currentSplit);
    const newIdx = (currentIdx + direction + allTabs.length) % allTabs.length;
    selectSplit(allTabs[newIdx]);
}

function selectSplitByIndex(index) {
    if (state.currentMailbox?.role !== 'inbox' || state.splits.length === 0) return;

    const allTabs = ['all', ...state.splits.map(s => s.id)];
    if (index >= 0 && index < allTabs.length) {
        selectSplit(allTabs[index]);
    }
}

async function loadMailboxes() {
    try {
        state.mailboxes = await api('GET', '/mailboxes');
        renderMailboxes();

        // Select inbox by default
        const inbox = state.mailboxes.find(m => m.role === 'inbox');
        if (inbox) selectMailbox(inbox);
    } catch (err) {
        showStatus('Failed to load mailboxes: ' + err.message, 'error');
    }
}

function buildEmailListUrl(mailboxId, { offset = 0, search = null } = {}) {
    let url = `/emails?mailbox_id=${mailboxId}&limit=${CACHE_LIMIT}`;
    if (offset > 0) url += `&offset=${offset}`;
    if (state.currentMailbox?.role === 'inbox' && state.currentSplit && state.currentSplit !== 'all' && state.splits.length > 0) {
        url += `&split_id=${state.currentSplit}`;
    }
    if (search) url += `&search=${encodeURIComponent(search)}`;
    return url;
}

async function loadEmails() {
    if (!state.currentMailbox) return;

    els.emailList.innerHTML = '<div class="loading">Loading</div>';

    try {
        const url = buildEmailListUrl(state.currentMailbox.id, { search: state.searchQuery });
        state.emails = await api('GET', url);
        state.selectedIndex = 0;
        renderEmailList();
        updateEmailCount();
    } catch (err) {
        showStatus('Failed to load emails: ' + err.message, 'error');
    }
}

async function maybeRefillEmails() {
    if (refillInFlight || state.emails.length >= REFILL_THRESHOLD) return;
    if (!state.currentMailbox) return;

    const mailboxId = state.currentMailbox.id;
    const searchQuery = state.searchQuery;

    refillInFlight = true;
    try {
        const url = buildEmailListUrl(mailboxId, { offset: state.emails.length, search: searchQuery });
        const fresh = await api('GET', url);

        // Discard results if context changed during fetch
        if (state.currentMailbox?.id !== mailboxId || state.searchQuery !== searchQuery) return;

        const existingIds = new Set(state.emails.map(e => e.id));
        const newEmails = fresh.filter(e => !existingIds.has(e.id));
        if (newEmails.length > 0) {
            state.emails = state.emails.concat(newEmails);
            renderEmailList();
            updateEmailCount();
        }
    } catch (err) {
        console.warn('Refill failed:', err);
    } finally {
        refillInFlight = false;
    }
}

async function loadEmailDetail(emailId) {
    // Save scroll position of the email we're leaving (if any)
    saveScrollPosition();

    // Use cache if available
    if (emailCache[emailId]) {
        state.currentEmail = emailCache[emailId];
        renderEmailDetail();
        els.emailBody.scrollTop = scrollPositions[emailId] || 0;
        showView('detail');
        return;
    }

    // Immediately hide calendar card from previous email while loading
    els.calendarEvent.classList.add('hidden');

    try {
        const email = await api('GET', `/emails/${emailId}`);
        emailCache[emailId] = email;  // Cache it
        state.currentEmail = email;
        renderEmailDetail();
        els.emailBody.scrollTop = 0;
        showView('detail');
    } catch (err) {
        showStatus('Failed to load email: ' + err.message, 'error');
    }
}

async function emailAction(type, emailId) {
    const label = type === 'archive' ? 'Archived' : 'Trashed';

    // Optimistic: remove from list and show feedback immediately
    const removedEmail = state.emails.find(e => e.id === emailId);
    const removedIndex = state.emails.indexOf(removedEmail);
    pushUndo(label.toLowerCase(), emailId);
    removeEmailFromList(emailId);
    showStatus(label, 'success');

    try {
        await api('POST', `/emails/${emailId}/${type}`);
    } catch (err) {
        // Revert: re-insert the email and remove the stale undo entry
        state.undoStack.pop();
        if (removedEmail) {
            state.emails.splice(removedIndex, 0, removedEmail);
            renderEmailList();
            updateEmailCount();
        }
        showStatus(label + ' failed: ' + err.message, 'error');
    }
}

async function toggleUnread(emailId) {
    const email = state.emails.find(e => e.id === emailId);
    if (!email) return;

    // Optimistic: toggle immediately
    const wasUnread = email.isUnread;
    email.isUnread = !wasUnread;
    renderEmailList();
    updateEmailCount();

    try {
        if (wasUnread) {
            await api('POST', `/emails/${emailId}/mark-read`);
        } else {
            await api('POST', `/emails/${emailId}/mark-unread`);
        }
    } catch (err) {
        // Revert
        email.isUnread = wasUnread;
        renderEmailList();
        updateEmailCount();
        showStatus('Failed to toggle read status', 'error');
    }
}

async function toggleFlag(emailId) {
    const email = state.emails.find(e => e.id === emailId);
    if (!email) return;

    // Optimistic: toggle immediately
    email.isFlagged = !email.isFlagged;
    renderEmailList();

    try {
        await api('POST', `/emails/${emailId}/toggle-flag`);
    } catch (err) {
        // Revert
        email.isFlagged = !email.isFlagged;
        renderEmailList();
        showStatus('Failed to toggle flag', 'error');
    }
}

async function sendEmail() {
    const to = els.composeTo.value.split(',').map(s => s.trim()).filter(Boolean);
    const cc = els.composeCc.value.split(',').map(s => s.trim()).filter(Boolean);
    const fromAddress = els.composeFrom?.value || null;
    const subject = els.composeSubject.value;
    const body = els.composeBody.value;

    if (!to.length) {
        showStatus('No recipients', 'error');
        return;
    }

    try {
        await api('POST', '/emails/send', {
            to,
            cc,
            subject,
            body,
            in_reply_to: state.replyContext?.inReplyTo || null,
            from_address: fromAddress,
        });
        showStatus('Sent!', 'success');
        clearCompose();
        showView('list');
    } catch (err) {
        showStatus('Send failed: ' + err.message, 'error');
    }
}

// Rendering

function renderMailboxes() {
    els.mailboxList.innerHTML = state.mailboxes
        .filter(m => m.role || m.parentId === null)
        .sort((a, b) => {
            const order = ['inbox', 'drafts', 'sent', 'archive', 'trash', 'spam'];
            const ai = order.indexOf(a.role) >= 0 ? order.indexOf(a.role) : 99;
            const bi = order.indexOf(b.role) >= 0 ? order.indexOf(b.role) : 99;
            return ai - bi;
        })
        .map(m => `
            <div class="mailbox-item ${state.currentMailbox?.id === m.id ? 'active' : ''}"
                 data-id="${m.id}">
                <span>${m.name}</span>
                ${m.unreadEmails > 0 ? `<span class="unread-count">${m.unreadEmails}</span>` : ''}
            </div>
        `).join('');

    els.mailboxList.querySelectorAll('.mailbox-item').forEach(el => {
        el.addEventListener('click', () => {
            const mb = state.mailboxes.find(m => m.id === el.dataset.id);
            if (mb) selectMailbox(mb);
        });
    });
}

function getRecipientBadge(email) {
    if (!email.to) return null;
    for (const split of state.splits) {
        for (const filt of split.filters) {
            if (filt.type !== 'to') continue;
            const addrs = [...(email.to || []), ...(email.cc || [])];
            for (const addr of addrs) {
                if (addr.email && addr.email.toLowerCase() === filt.pattern.toLowerCase()) {
                    return split.name;
                }
            }
        }
    }
    return null;
}

function renderEmailList() {
    if (!state.emails.length) {
        els.emailList.innerHTML = '<div class="empty-state">No emails</div>';
        return;
    }

    const showBadge = state.currentSplit === 'all';
    let lastGroup = null;

    els.emailList.innerHTML = state.emails.map((email, idx) => {
        const from = email.from[0];
        const fromDisplay = from?.name || from?.email || 'Unknown';
        const date = formatDate(email.receivedAt);
        const badge = showBadge ? getRecipientBadge(email) : null;
        const group = getDateGroup(email.receivedAt);
        let divider = '';
        if (group !== lastGroup) {
            lastGroup = group;
            divider = `<div class="date-divider"><span class="date-divider-label">${group}</span></div>`;
        }

        return divider + `
            <div class="email-row ${idx === state.selectedIndex ? 'selected' : ''} ${email.isUnread ? 'unread' : ''}"
                 data-id="${email.id}" data-index="${idx}">
                <span class="email-flag ${email.isFlagged ? 'flagged' : ''}">${email.isFlagged ? '★' : '☆'}</span>
                <span class="email-from">${escapeHtml(fromDisplay)}</span>
                ${badge ? `<span class="email-recipient-badge">${escapeHtml(badge)}</span>` : ''}
                <span class="email-subject">
                    ${escapeHtml(email.subject)}
                    <span class="email-preview">— ${escapeHtml(email.preview)}</span>
                </span>
                ${email.hasAttachment ? '<span class="email-attachment">📎</span>' : ''}
                <span class="email-date">${date}</span>
            </div>
        `;
    }).join('');

    els.emailList.querySelectorAll('.email-row').forEach(el => {
        el.addEventListener('click', () => {
            state.selectedIndex = parseInt(el.dataset.index);
            renderEmailList();
            loadEmailDetail(el.dataset.id);
        });
    });

    scrollSelectedIntoView();
}

function renderEmailDetail() {
    if (!state.currentEmail) return;

    const e = state.currentEmail;
    const from = e.from[0];
    const fromDisplay = from?.name ? `${from.name} <${from.email}>` : from?.email || 'Unknown';
    const toDisplay = e.to.map(t => t.name || t.email).join(', ');
    const date = new Date(e.receivedAt).toLocaleString();

    els.emailSubject.textContent = e.subject;
    els.emailMeta.innerHTML = `
        <div><span class="label">From:</span> ${escapeHtml(fromDisplay)}</div>
        <div><span class="label">To:</span> ${escapeHtml(toDisplay)}</div>
        <div><span class="label">Date:</span> ${date}</div>
    `;

    // Render calendar event if present
    if (e.calendarEvent) {
        renderCalendarCard(e.calendarEvent);
    } else {
        els.calendarEvent.classList.add('hidden');
    }

    if (e.htmlBody) {
        els.emailBody.innerHTML = sanitizeHtml(e.htmlBody);
        els.emailBody.classList.add('html-content');
    } else {
        els.emailBody.innerHTML = linkifyText(e.textBody || '(no content)');
        els.emailBody.classList.remove('html-content');
    }
}

function renderCommandPalette() {
    const commands = getCommands();
    const query = els.commandInput.value.toLowerCase();
    const filtered = commands.filter(c =>
        c.name.toLowerCase().includes(query) ||
        c.desc.toLowerCase().includes(query)
    );

    els.commandResults.innerHTML = filtered.map((cmd, idx) => `
        <div class="command-item ${idx === state.commandPaletteIndex ? 'selected' : ''}"
             data-action="${cmd.action}">
            <span>${cmd.name}</span>
            <span class="shortcut">${cmd.shortcut}</span>
        </div>
    `).join('');

    els.commandResults.querySelectorAll('.command-item').forEach(el => {
        el.addEventListener('click', () => {
            executeCommand(el.dataset.action);
            closeCommandPalette();
        });
    });
}

// View management

function saveScrollPosition() {
    if (state.view === 'detail' && state.currentEmail) {
        scrollPositions[state.currentEmail.id] = els.emailBody.scrollTop;
    }
}

function showView(view) {
    if (state.view === 'detail' && view !== 'detail') {
        saveScrollPosition();
    }
    state.view = view;
    els.emailListView.classList.toggle('active', view === 'list');
    els.emailDetailView.classList.toggle('active', view === 'detail');
    els.composeView.classList.toggle('active', view === 'compose');

    if (view === 'compose') {
        els.composeTo.focus();
    }
}

function selectMailbox(mailbox) {
    state.currentMailbox = mailbox;
    state.searchQuery = '';
    state.currentSplit = mailbox.role === 'inbox' ? 'all' : null;
    els.mailboxName.textContent = mailbox.name.toUpperCase();
    renderMailboxes();
    renderSplitTabs();
    loadEmails();
}

function setMode(mode) {
    state.mode = mode;
    els.modeIndicator.textContent = mode.toUpperCase();
    els.modeIndicator.className = mode;
}

function showStatus(message, type = 'info') {
    els.statusMessage.textContent = message;
    els.statusMessage.style.color = type === 'error' ? 'var(--danger)' :
                                    type === 'success' ? 'var(--success)' : 'var(--fg-muted)';
    setTimeout(() => {
        els.statusMessage.textContent = '';
    }, 3000);
}

function updateEmailCount() {
    const total = state.emails.length;
    const unread = state.emails.filter(e => e.isUnread).length;
    els.emailCount.textContent = unread > 0 ? `${unread}/${total}` : `${total}`;
}

// Keyboard handling

function handleKeyDown(e) {
    // Handle help overlay
    if (!els.helpOverlay.classList.contains('hidden')) {
        els.helpOverlay.classList.add('hidden');
        e.preventDefault();
        return;
    }

    // Handle command palette
    if (!els.commandPalette.classList.contains('hidden')) {
        handleCommandPaletteKey(e);
        return;
    }

    // Handle search
    if (!els.searchBar.classList.contains('hidden')) {
        return; // Let search input handle it
    }

    // Handle split modal
    if (!els.splitModal.classList.contains('hidden')) {
        if (e.key === 'Escape') {
            closeSplitModal();
            e.preventDefault();
        } else if (e.key === 'Enter' && e.ctrlKey) {
            saveSplit();
            e.preventDefault();
        }
        return;
    }

    // Handle compose mode
    if (state.view === 'compose' && state.mode === 'insert') {
        if (e.key === 'Escape') {
            e.target.blur();
            setMode('normal');
            e.preventDefault();
        } else if (e.key === 'Enter' && e.ctrlKey) {
            sendEmail();
            e.preventDefault();
        }
        return;
    }

    // Ctrl+1-9: jump to split tab
    if (e.ctrlKey && e.key >= '1' && e.key <= '9') {
        selectSplitByIndex(parseInt(e.key) - 1);
        e.preventDefault();
        return;
    }

    // Command palette shortcut
    if ((e.metaKey || e.ctrlKey) && e.key === 'k') {
        openCommandPalette();
        e.preventDefault();
        return;
    }

    // Normal mode keys
    if (state.mode === 'normal') {
        handleNormalModeKey(e);
    }
}

function handleNormalModeKey(e) {
    const key = e.key;

    // Handle gg sequence
    if (state.pendingG) {
        state.pendingG = false;
        if (key === 'g') {
            moveToTop();
            return;
        }
    }

    switch (key) {
        // Page scrolling in detail view
        case ' ':
            if (state.view === 'detail') {
                const scrollEl = els.emailBody;
                if (e.shiftKey) {
                    scrollEl.scrollBy({ top: -scrollEl.clientHeight, behavior: 'instant' });
                } else {
                    scrollEl.scrollBy({ top: scrollEl.clientHeight, behavior: 'instant' });
                }
                e.preventDefault();
                return;
            }
            break;

        // Navigation
        case 'j':
            moveSelection(1);
            break;
        case 'k':
            moveSelection(-1);
            break;
        case 'g':
            state.pendingG = true;
            setTimeout(() => { state.pendingG = false; }, 500);
            break;
        case 'G':
            moveToBottom();
            break;
        case 'Enter':
        case 'o':
            openSelected();
            break;
        case 'Escape':
        case 'q':
            if (state.view === 'detail') {
                showView('list');
            } else if (state.view === 'compose') {
                clearCompose();
                showView('list');
            }
            break;

        // Actions
        case 'e':
            actionSelected('archive');
            break;
        case '#':
            actionSelected('trash');
            break;
        case 'r':
            startReply(false);
            break;
        case 'a':
            startReply(true);
            break;
        case 'c':
            startCompose();
            break;
        case 'f':
            startForward();
            break;
        case 'u':
            toggleUnreadSelected();
            break;
        case 'U':
            unsubscribeAndArchiveAll();
            break;
        case 's':
            toggleFlagSelected();
            break;
        case 'z':
            performUndo();
            break;

        // Search
        case '/':
            openSearch();
            e.preventDefault();
            break;

        // Other
        case '?':
            els.helpOverlay.classList.remove('hidden');
            break;
        case 'R':
            loadEmails();
            showStatus('Refreshing...', 'info');
            break;

        // Split tab cycling
        case 'Tab':
            if (e.shiftKey) {
                cycleSplit(-1);
            } else {
                cycleSplit(1);
            }
            e.preventDefault();
            break;

        // Account switching (1-9)
        case '1': case '2': case '3': case '4': case '5':
        case '6': case '7': case '8': case '9':
            const accIndex = parseInt(key) - 1;
            if (accIndex < state.accounts.length) {
                selectAccount(state.accounts[accIndex]);
                showStatus(`Switched to ${state.accounts[accIndex].email}`, 'success');
            }
            break;
    }
}

function handleCommandPaletteKey(e) {
    if (e.key === 'Escape') {
        closeCommandPalette();
        e.preventDefault();
    } else if (e.key === 'Enter') {
        const selected = els.commandResults.querySelector('.command-item.selected');
        if (selected) {
            executeCommand(selected.dataset.action);
        }
        closeCommandPalette();
        e.preventDefault();
    } else if (e.key === 'ArrowDown') {
        state.commandPaletteIndex++;
        renderCommandPalette();
        e.preventDefault();
    } else if (e.key === 'ArrowUp') {
        state.commandPaletteIndex = Math.max(0, state.commandPaletteIndex - 1);
        renderCommandPalette();
        e.preventDefault();
    }
}

function handleCommandInput() {
    state.commandPaletteIndex = 0;
    renderCommandPalette();
}

function handleSearchKeyDown(e) {
    if (e.key === 'Enter') {
        state.searchQuery = els.searchInput.value;
        closeSearch();
        loadEmails();
    } else if (e.key === 'Escape') {
        closeSearch();
    }
}

// Navigation actions

function moveSelection(delta) {
    const newIndex = state.selectedIndex + delta;
    if (newIndex < 0 || newIndex >= state.emails.length) return;
    state.selectedIndex = newIndex;
    renderEmailList();
    if (state.view === 'detail') {
        loadEmailDetail(state.emails[state.selectedIndex].id);
    }
}

function moveToTop() {
    state.selectedIndex = 0;
    renderEmailList();
}

function moveToBottom() {
    state.selectedIndex = Math.max(0, state.emails.length - 1);
    renderEmailList();
}

function openSelected() {
    const email = state.emails[state.selectedIndex];
    if (email) {
        loadEmailDetail(email.id);
    }
}

function scrollSelectedIntoView() {
    const selected = els.emailList.querySelector('.email-row.selected');
    if (selected) {
        selected.scrollIntoView({ block: 'nearest' });
    }
}

// Email actions

function getSelectedEmailId() {
    if (state.view === 'detail' && state.currentEmail) {
        return state.currentEmail.id;
    }
    const email = state.emails[state.selectedIndex];
    return email?.id;
}

function actionSelected(type) {
    const id = getSelectedEmailId();
    if (id) {
        emailAction(type, id);
        if (state.view === 'detail') {
            goToNextEmail();
        }
    }
}

function goToNextEmail() {
    const currentId = state.currentEmail?.id;
    const currentIndex = state.emails.findIndex(e => e.id === currentId);

    // Remove from list if still present (may already be removed by optimistic emailAction)
    if (currentIndex >= 0) {
        state.emails.splice(currentIndex, 1);
        renderEmailList();
        updateEmailCount();
    }

    if (state.emails.length === 0) {
        showView('list');
        maybeRefillEmails();
        return;
    }

    // Use currentIndex if we removed it here, otherwise fall back to selectedIndex
    const baseIndex = currentIndex >= 0 ? currentIndex : state.selectedIndex;
    const nextIndex = Math.min(baseIndex, state.emails.length - 1);
    state.selectedIndex = nextIndex;
    const nextEmail = state.emails[nextIndex];

    if (nextEmail) {
        loadEmailDetail(nextEmail.id);
    } else {
        showView('list');
    }
    maybeRefillEmails();
}

function toggleUnreadSelected() {
    const id = getSelectedEmailId();
    if (id) toggleUnread(id);
}

function toggleFlagSelected() {
    const id = getSelectedEmailId();
    if (id) toggleFlag(id);
}

async function unsubscribeAndArchiveAll() {
    const id = getSelectedEmailId();
    if (!id) return;

    showStatus('Unsubscribing and archiving...', 'info');

    try {
        const result = await api('POST', `/emails/${id}/unsubscribe-and-archive-all`);

        if (result.unsubscribeUrl) {
            // Open unsubscribe link in new tab
            window.open(result.unsubscribeUrl, '_blank');
            showStatus(`Archived ${result.archivedCount} emails from ${result.sender}. Unsubscribe page opened.`, 'success');
        } else {
            showStatus(`Archived ${result.archivedCount} emails from ${result.sender}. No unsubscribe link found.`, 'warning');
        }

        // Refresh and go to next email
        if (state.view === 'detail') {
            await loadEmails();
            goToNextEmail();
        } else {
            loadEmails();
        }
    } catch (err) {
        showStatus('Unsubscribe failed: ' + err.message, 'error');
    }
}

function removeEmailFromList(emailId) {
    state.emails = state.emails.filter(e => e.id !== emailId);
    if (state.selectedIndex >= state.emails.length) {
        state.selectedIndex = Math.max(0, state.emails.length - 1);
    }
    renderEmailList();
    updateEmailCount();
    maybeRefillEmails();
}

// Compose

function startCompose() {
    state.replyContext = null;
    clearCompose();
    showView('compose');
}

function startReply(replyAll) {
    const email = state.view === 'detail' ? state.currentEmail : state.emails[state.selectedIndex];
    if (!email) return;

    clearCompose();

    const from = email.from[0];
    els.composeTo.value = from?.email || '';

    if (replyAll && email.to) {
        const others = email.to
            .filter(t => t.email)
            .map(t => t.email)
            .join(', ');
        els.composeCc.value = others;
    }

    els.composeSubject.value = email.subject.startsWith('Re:') ? email.subject : `Re: ${email.subject}`;

    state.replyContext = {
        inReplyTo: email.id,
    };

    // Auto-select the identity matching the To/CC of the original email
    autoSelectFromAddress(email);

    const quote = email.textBody || '';
    const quotedLines = quote.split('\n').map(line => '> ' + line).join('\n');
    els.composeBody.value = `\n\n${quotedLines}`;

    showView('compose');
}

function startForward() {
    const email = state.view === 'detail' ? state.currentEmail : state.emails[state.selectedIndex];
    if (!email) return;

    clearCompose();

    els.composeSubject.value = email.subject.startsWith('Fwd:') ? email.subject : `Fwd: ${email.subject}`;

    const from = email.from[0];
    const header = `---------- Forwarded message ---------\nFrom: ${from?.name || ''} <${from?.email}>\nSubject: ${email.subject}\n\n`;
    els.composeBody.value = header + (email.textBody || '');

    showView('compose');
}

function clearCompose() {
    els.composeTo.value = '';
    els.composeCc.value = '';
    els.composeSubject.value = '';
    els.composeBody.value = '';
    if (els.composeFrom && state.identities.length) {
        els.composeFrom.value = state.identities[0].email;
    }
    state.replyContext = null;
}

function autoSelectFromAddress(email) {
    if (!els.composeFrom || !state.identities.length) return;
    const myAddresses = new Set(state.identities.map(i => i.email.toLowerCase()));
    const recipients = [...(email.to || []), ...(email.cc || [])];
    for (const addr of recipients) {
        if (addr.email && myAddresses.has(addr.email.toLowerCase())) {
            els.composeFrom.value = addr.email;
            return;
        }
    }
}

// Command palette

function openCommandPalette() {
    els.commandPalette.classList.remove('hidden');
    els.commandInput.value = '';
    state.commandPaletteIndex = 0;
    renderCommandPalette();
    els.commandInput.focus();
    setMode('command');
}

function closeCommandPalette() {
    els.commandPalette.classList.add('hidden');
    setMode('normal');
}

function getCommands() {
    const commands = [
        { name: 'Archive', desc: 'Archive email', shortcut: 'e', action: 'archive' },
        { name: 'Trash', desc: 'Move to trash', shortcut: '#', action: 'trash' },
        { name: 'Reply', desc: 'Reply to sender', shortcut: 'r', action: 'reply' },
        { name: 'Reply All', desc: 'Reply to all', shortcut: 'a', action: 'reply-all' },
        { name: 'Compose', desc: 'New email', shortcut: 'c', action: 'compose' },
        { name: 'Forward', desc: 'Forward email', shortcut: 'f', action: 'forward' },
        { name: 'Mark Unread', desc: 'Toggle unread', shortcut: 'u', action: 'toggle-unread' },
        { name: 'Star', desc: 'Toggle star', shortcut: 's', action: 'toggle-flag' },
        { name: 'Refresh', desc: 'Reload emails', shortcut: 'R', action: 'refresh' },
        { name: 'Go to Inbox', desc: 'Switch to inbox', shortcut: '', action: 'inbox' },
        { name: 'Go to Archive', desc: 'Switch to archive', shortcut: '', action: 'go-archive' },
        { name: 'Go to Trash', desc: 'Switch to trash', shortcut: '', action: 'go-trash' },
        { name: 'New Split', desc: 'Create split inbox', shortcut: '', action: 'new-split' },
        { name: 'Help', desc: 'Show shortcuts', shortcut: '?', action: 'help' },
    ];

    // Add delete commands for each existing split
    state.splits.forEach(split => {
        commands.push({
            name: `Delete Split: ${split.name}`,
            desc: `Remove the "${split.name}" split`,
            shortcut: '',
            action: `delete-split:${split.id}`,
        });
    });

    return commands;
}

function executeCommand(action) {
    switch (action) {
        case 'archive': actionSelected('archive'); break;
        case 'trash': actionSelected('trash'); break;
        case 'reply': startReply(false); break;
        case 'reply-all': startReply(true); break;
        case 'compose': startCompose(); break;
        case 'forward': startForward(); break;
        case 'toggle-unread': toggleUnreadSelected(); break;
        case 'toggle-flag': toggleFlagSelected(); break;
        case 'refresh': loadEmails(); break;
        case 'inbox': {
            const inbox = state.mailboxes.find(m => m.role === 'inbox');
            if (inbox) selectMailbox(inbox);
            break;
        }
        case 'go-archive': {
            const archive = state.mailboxes.find(m => m.role === 'archive');
            if (archive) selectMailbox(archive);
            break;
        }
        case 'go-trash': {
            const trash = state.mailboxes.find(m => m.role === 'trash');
            if (trash) selectMailbox(trash);
            break;
        }
        case 'help':
            els.helpOverlay.classList.remove('hidden');
            break;
        case 'new-split':
            openSplitModal();
            break;
        default:
            // Handle dynamic delete-split commands
            if (action.startsWith('delete-split:')) {
                const splitId = action.replace('delete-split:', '');
                deleteSplit(splitId);
            }
            break;
    }
}

// Search

function openSearch() {
    els.searchBar.classList.remove('hidden');
    els.searchInput.value = state.searchQuery;
    els.searchInput.focus();
    setMode('command');
}

function closeSearch() {
    els.searchBar.classList.add('hidden');
    setMode('normal');
}

// Split management

function openSplitModal() {
    els.splitName.value = '';
    els.splitFilterType.value = 'from';
    els.splitPattern.value = '';
    updateSplitModalFields();
    els.splitModal.classList.remove('hidden');
    els.splitName.focus();
    setMode('insert');
}

function closeSplitModal() {
    els.splitModal.classList.add('hidden');
    setMode('normal');
}

function updateSplitModalFields() {
    const filterType = els.splitFilterType.value;
    const isCalendar = filterType === 'calendar';

    // hide pattern field for calendar type (no pattern needed)
    els.splitPatternField.style.display = isCalendar ? 'none' : 'block';

    // update hint text
    if (isCalendar) {
        els.splitHint.textContent = 'Matches all emails with iCalendar (ICS) attachments.';
    } else if (filterType === 'from') {
        els.splitHint.textContent = 'Use * as wildcard. e.g., *@calendar.google.com';
    } else if (filterType === 'to') {
        els.splitHint.textContent = 'Use * as wildcard. e.g., *@aristoi.ai';
    } else {
        els.splitHint.textContent = 'Use regex pattern. e.g., newsletter|digest';
    }
}

async function saveSplit() {
    const name = els.splitName.value.trim();
    const filterType = els.splitFilterType.value;
    const pattern = els.splitPattern.value.trim();
    const isCalendar = filterType === 'calendar';

    if (!name) {
        showStatus('Name is required', 'error');
        return;
    }

    if (!isCalendar && !pattern) {
        showStatus('Pattern is required', 'error');
        return;
    }

    // Generate ID from name (lowercase, no spaces)
    const id = name.toLowerCase().replace(/\s+/g, '-').replace(/[^a-z0-9-]/g, '');

    // Build filter - calendar type doesn't need a pattern
    const filter = isCalendar
        ? { type: filterType, pattern: 'true' }  // dummy pattern, not used
        : { type: filterType, pattern };

    try {
        await api('POST', '/splits', {
            id,
            name,
            filters: [filter],
            match_mode: 'any',
        });

        showStatus(`Split "${name}" created`, 'success');
        closeSplitModal();
        await loadSplits();

        // If we're in inbox, show the tabs
        if (state.currentMailbox?.role === 'inbox') {
            renderSplitTabs();
        }
    } catch (err) {
        showStatus('Failed to create split: ' + err.message, 'error');
    }
}

async function deleteSplit(splitId) {
    const split = state.splits.find(s => s.id === splitId);
    if (!split) return;

    try {
        await api('DELETE', `/splits/${splitId}`);
        showStatus(`Split "${split.name}" deleted`, 'success');
        await loadSplits();

        // Reset to all if we deleted the current split
        if (state.currentSplit === splitId) {
            state.currentSplit = 'all';
        }
        renderSplitTabs();
        loadEmails();
    } catch (err) {
        showStatus('Failed to delete split: ' + err.message, 'error');
    }
}

// Undo

function pushUndo(action, emailId) {
    state.undoStack.push({ action, emailId, timestamp: Date.now() });

    // Show toast
    els.undoMessage.textContent = action === 'archived' ? 'Email archived' : 'Email trashed';
    els.undoToast.classList.remove('hidden');

    // Auto-hide after 5 seconds
    setTimeout(() => {
        els.undoToast.classList.add('hidden');
    }, 5000);
}

async function performUndo() {
    const item = state.undoStack.pop();
    if (!item) return;

    // Optimistic: hide toast and show feedback immediately
    els.undoToast.classList.add('hidden');
    showStatus('Undone', 'success');

    try {
        const inbox = state.mailboxes.find(m => m.role === 'inbox');
        if (inbox) {
            await api('POST', `/emails/${item.emailId}/move`, { mailbox_id: inbox.id });
            loadEmails();
        }
    } catch (err) {
        showStatus('Undo failed', 'error');
    }
}

// Utilities

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

    // Start of this week (Monday)
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

// Strip color-related CSS properties from inline styles.
// Preserves layout (margin, padding, display) while removing colors.
function stripColorStyles(styleString) {
    const colorProps = [
        'color', 'background', 'background-color', 'background-image',
        'border-color', 'border-top-color', 'border-right-color',
        'border-bottom-color', 'border-left-color', 'outline-color',
        'text-decoration-color', 'text-shadow', 'box-shadow'
    ];
    return styleString.split(';')
        .map(d => d.trim())
        .filter(d => {
            if (!d) return false;
            const propName = d.split(':')[0]?.trim().toLowerCase();
            return propName && !colorProps.some(cp => propName === cp || propName.startsWith(cp + '-'));
        })
        .join('; ');
}

function sanitizeStyleContent(css) {
    // Remove @import rules (external resource loading / tracking)
    css = css.replace(/@import\b[^;]*;?/gi, '');
    // Remove @font-face rules (external resource loading)
    css = css.replace(/@font-face\s*\{[^}]*\}/gi, '');
    // Remove url() references (external resource loading / tracking)
    css = css.replace(/url\s*\([^)]*\)/gi, '');
    // Remove expression() (IE CSS expressions)
    css = css.replace(/expression\s*\([^)]*\)/gi, '');
    // Remove -moz-binding (Firefox XBL)
    css = css.replace(/-moz-binding\s*:[^;]+;?/gi, '');
    // Remove behavior (IE HTC)
    css = css.replace(/behavior\s*:[^;]+;?/gi, '');
    return css;
}

function scopeStyleToEmailBody(css) {
    // Prefix all CSS selectors with #email-body to prevent leaking into app UI
    return css.replace(
        /([^{}@]+)\{/g,
        (match, selectors) => {
            // Don't modify @-rules (@media, @keyframes, etc.)
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

function sanitizeHtml(html) {
    const doc = new DOMParser().parseFromString(html, 'text/html');

    // Remove dangerous elements (style kept and sanitized separately)
    const dangerousTags = ['script', 'iframe', 'object', 'embed', 'form', 'input', 'button', 'meta', 'base', 'link', 'svg', 'math'];
    dangerousTags.forEach(tag => {
        doc.querySelectorAll(tag).forEach(el => el.remove());
    });

    // Sanitize and scope style elements
    doc.querySelectorAll('style').forEach(el => {
        el.textContent = scopeStyleToEmailBody(sanitizeStyleContent(el.textContent));
    });

    // Sanitize all elements
    doc.querySelectorAll('*').forEach(el => {
        // Remove legacy HTML color attributes
        if (el.hasAttribute('bgcolor')) el.removeAttribute('bgcolor');
        if (el.hasAttribute('color')) el.removeAttribute('color');

        const attrs = [...el.attributes];
        attrs.forEach(attr => {
            const name = attr.name.toLowerCase();
            const value = attr.value.toLowerCase();

            // Remove event handlers
            if (name.startsWith('on')) {
                el.removeAttribute(attr.name);
                return;
            }

            // Remove dangerous URL schemes in href/src/action
            if (['href', 'src', 'action', 'xlink:href', 'formaction'].includes(name)) {
                if (value.startsWith('javascript:') || value.startsWith('vbscript:')) {
                    el.removeAttribute(attr.name);
                }
                // Block data: URLs except for images
                if (value.startsWith('data:') && !value.startsWith('data:image/')) {
                    el.removeAttribute(attr.name);
                }
            }

            // Strip color styles, remove dangerous expressions
            if (name === 'style') {
                if (value.includes('expression') || value.includes('javascript')) {
                    el.removeAttribute(attr.name);
                    return;
                }
                const cleaned = stripColorStyles(attr.value);
                if (cleaned) {
                    el.setAttribute('style', cleaned);
                } else {
                    el.removeAttribute('style');
                }
            }
        });
    });

    // Make all links open in a new tab
    doc.querySelectorAll('a[href]').forEach(el => {
        el.setAttribute('target', '_blank');
        el.setAttribute('rel', 'noopener noreferrer');
    });

    return doc.body.innerHTML;
}

function linkifyText(text) {
    const escaped = escapeHtml(text);
    return escaped.replace(
        /https?:\/\/[^\s<>&"')\]]+/g,
        url => {
            const trimmed = url.replace(/[.,;:!?]+$/, '');
            const suffix = url.slice(trimmed.length);
            return `<a href="${trimmed}" target="_blank" rel="noopener noreferrer">${trimmed}</a>${suffix}`;
        }
    );
}

// Calendar functions

function renderCalendarCard(event) {
    els.calendarEvent.classList.remove('hidden');
    const cancelled = event.method === 'CANCEL';
    const card = els.calendarEvent.querySelector('.calendar-card');
    card.classList.toggle('cancelled', cancelled);

    els.calTitle.textContent = event.summary || 'Calendar Event';
    els.calDatetime.textContent = formatEventTime(event.dtstart, event.dtend);
    els.calLocation.textContent = event.location || '';
    els.calLocation.style.display = event.location ? 'block' : 'none';

    // Show/hide cancelled banner
    let banner = els.calendarEvent.querySelector('.cal-cancelled');
    if (cancelled) {
        if (!banner) {
            banner = document.createElement('div');
            banner.className = 'cal-cancelled';
            banner.textContent = 'CANCELLED';
            card.querySelector('.cal-header').after(banner);
        }
    } else if (banner) {
        banner.remove();
    }

    // Render attendees
    if (event.attendees && event.attendees.length > 0) {
        const attendeeList = event.attendees.map(a => {
            const name = a.name || a.email;
            const statusIcon = getStatusIcon(a.status);
            return `<span class="attendee" title="${a.email}">${statusIcon} ${escapeHtml(name)}</span>`;
        }).join(', ');
        els.calAttendees.innerHTML = `<span class="label">Attendees:</span> ${attendeeList}`;
        els.calAttendees.style.display = 'block';
    } else {
        els.calAttendees.style.display = 'none';
    }

    // Hide RSVP actions for cancelled events
    const actions = els.calendarEvent.querySelector('.calendar-actions');
    if (cancelled) {
        actions.style.display = 'none';
    } else {
        actions.style.display = '';
        // Find current user's RSVP status and highlight active button
        const userStatus = getUserRsvpStatus(event);
        els.rsvpAccept.classList.toggle('active', userStatus === 'ACCEPTED');
        els.rsvpMaybe.classList.toggle('active', userStatus === 'TENTATIVE');
        els.rsvpDecline.classList.toggle('active', userStatus === 'DECLINED');

        // Hide "Add to Calendar" if already accepted/tentative
        els.calAdd.style.display = (userStatus === 'ACCEPTED' || userStatus === 'TENTATIVE') ? 'none' : '';
    }
}

function getUserRsvpStatus(event) {
    if (!event.attendees || !state.currentAccount) return null;
    const accountEmail = state.currentAccount.email?.toLowerCase();
    for (const a of event.attendees) {
        if (a.email.toLowerCase() === accountEmail) return a.status;
    }
    // Also check To/CC of current email for matching attendee
    if (state.currentEmail) {
        const toEmails = [...(state.currentEmail.to || []), ...(state.currentEmail.cc || [])].map(t => t.email?.toLowerCase());
        for (const a of event.attendees) {
            if (toEmails.includes(a.email.toLowerCase())) return a.status;
        }
    }
    return null;
}

function formatEventTime(dtstart, dtend) {
    if (!dtstart) return '';
    const start = new Date(dtstart);
    const options = {
        weekday: 'short',
        month: 'short',
        day: 'numeric',
        hour: 'numeric',
        minute: '2-digit'
    };
    let result = start.toLocaleString(undefined, options);

    if (dtend) {
        const end = new Date(dtend);
        // If same day, just show end time
        if (start.toDateString() === end.toDateString()) {
            result += ' - ' + end.toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit' });
        } else {
            result += ' - ' + end.toLocaleString(undefined, options);
        }
    }
    return result;
}

function getStatusIcon(status) {
    switch (status) {
        case 'ACCEPTED': return '<span class="status-icon accepted">&#10003;</span>';
        case 'DECLINED': return '<span class="status-icon declined">&#10007;</span>';
        case 'TENTATIVE': return '<span class="status-icon tentative">?</span>';
        default: return '<span class="status-icon pending">&#8226;</span>';
    }
}

async function rsvpToEvent(status) {
    if (!state.currentEmail) return;

    const label = { ACCEPTED: 'Accepted', TENTATIVE: 'Maybe', DECLINED: 'Declined' }[status] || status;
    const event = state.currentEmail.calendarEvent;
    let prevEvent = null;

    // Optimistic: update RSVP buttons immediately if we have event data
    if (event) {
        prevEvent = JSON.parse(JSON.stringify(event));
        const accountEmail = state.currentAccount?.email?.toLowerCase();
        if (accountEmail && event.attendees) {
            for (const a of event.attendees) {
                if (a.email.toLowerCase() === accountEmail) {
                    a.status = status;
                    break;
                }
            }
        }
        renderCalendarCard(event);
    }
    showStatus(`RSVP: ${label}`, 'success');

    try {
        const result = await api('POST', `/emails/${state.currentEmail.id}/rsvp`, { status });
        if (result.calendarEvent) {
            state.currentEmail.calendarEvent = result.calendarEvent;
            emailCache[state.currentEmail.id] = state.currentEmail;
            renderCalendarCard(result.calendarEvent);
        }
    } catch (err) {
        // Revert optimistic update if we had one
        if (prevEvent) {
            state.currentEmail.calendarEvent = prevEvent;
            emailCache[state.currentEmail.id] = state.currentEmail;
            renderCalendarCard(prevEvent);
        }
        showStatus('Failed to send RSVP: ' + err.message, 'error');
    }
}

async function addToCalendar() {
    if (!state.currentEmail) return;

    try {
        await api('POST', `/emails/${state.currentEmail.id}/add-to-calendar`);
        showStatus('Event added to calendar', 'success');
    } catch (err) {
        showStatus('Failed to add to calendar: ' + err.message, 'error');
    }
}

// Initialize on load
document.addEventListener('DOMContentLoaded', init);

// Supervillain - The open anti-superhuman email client
// Direct, readable code. No framework, no build step.

const state = {
    mode: 'normal',           // normal, insert, command, search, awaiting
    view: 'list',             // list, detail, compose, settings
    accounts: [],
    currentAccount: null,
    mailboxes: [],
    currentMailbox: null,
    emails: [],
    selectedIndex: 0,
    currentEmail: null,
    searchTokens: [],
    autocompleteIndex: 0,
    undoStack: [],
    pendingG: false,          // for gg command
    commandPaletteIndex: 0,
    replyContext: null,       // for reply/forward
    identities: [],           // send-as email addresses
    splits: [],               // split inbox definitions
    currentSplit: 'all',      // currently active split tab
    pendingAttachments: [],   // files being uploaded for compose
    splitCounts: {},          // email counts per split tab
    starredOnly: false,       // sidebar "Starred" filter — restricts list to $flagged emails
    // Settings view (account management)
    selectedAccountId: null,  // which account is focused in settings
    settingsMode: 'view',     // 'view' | 'edit' | 'awaiting'
    authController: null,     // AbortController for the in-flight authorize fetch
    // Add-account wizard (4-step). Active only while adding a new account;
    // existing-account edits keep using the dense form.
    wizardActive: false,
    wizardStep: 1,
    wizardProviderIdx: 0,     // 0=gmail, 1=outlook, 2=fastmail
    wizardSavedId: null,      // id of the account being created (set after step 2 save)
    // In-memory cache of typed wizard fields, keyed by provider. Survives
    // step transitions and wizard reopen within a page session so the user
    // doesn't re-type after esc-back or cancelled OAuth. Cleared on page
    // reload and on wizFinish for the provider just completed. Uniform
    // shape across providers (see freshWizCache).
    wizardCache: null,  // populated at init() once freshWizCache is defined
    timezone: null,           // { primary, display, system, system_changed, use_system, ... }
    tzZones: [],              // cached list of IANA names from /api/timezone/zones
};

// Only these top-level path prefixes (after /api) get the ?account= query
// param auto-appended. Settings routes are GLOBAL and must not be tagged.
const ACCOUNT_SCOPED_API = /^\/(emails|mailboxes|identities|splits|upload|split-counts|calendar)/;

// Simple cache: email id -> full email object with body
const emailCache = {};
// Scroll position cache: email id -> scrollTop
const scrollPositions = {};

// Rolling email cache
const CACHE_LIMIT = 150;
const REFILL_THRESHOLD = 100;
let refillInFlight = false;

// Per-split email list cache for instant split switching
// Key: "accountId:mailboxId:splitId:search" -> email array
const splitListCache = {};
let loadEmailsController = null;

const SEARCH_OPERATORS = [
    { op: 'from:', hint: 'Sender email', needsValue: true },
    { op: 'to:', hint: 'Recipient', needsValue: true },
    { op: 'subject:', hint: 'Subject line', needsValue: true },
    { op: 'has:attachment', hint: 'Has attachments', needsValue: false },
    { op: 'is:unread', hint: 'Unread only', needsValue: false },
    { op: 'is:read', hint: 'Read only', needsValue: false },
    { op: 'is:starred', hint: 'Starred only', needsValue: false },
    { op: 'newer_than:', hint: '7d, 2w, 3m, or MM-DD-YY', needsValue: true },
    { op: 'older_than:', hint: '7d, 2w, 3m, or MM-DD-YY', needsValue: true },
    { op: 'before:', hint: 'YYYY-MM-DD', needsValue: true },
    { op: 'after:', hint: 'YYYY-MM-DD', needsValue: true },
];

// DOM elements
const els = {};

function init() {
    // Wizard cache — uniform shape per provider; reset to fresh on finish.
    state.wizardCache = Object.fromEntries(
        WIZ_PROVIDERS.map(p => [p, freshWizCache()])
    );

    // Cache DOM elements
    els.modeIndicator = document.getElementById('mode-indicator');
    els.mailboxName = document.getElementById('mailbox-name');
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
    els.searchTokens = document.getElementById('search-tokens');
    els.searchAutocomplete = document.getElementById('search-autocomplete');
    els.activeFilters = document.getElementById('active-filters');
    els.activeFilterChips = document.getElementById('active-filter-chips');
    els.clearAllFilters = document.getElementById('clear-all-filters');
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
    els.attachments = document.getElementById('attachments');
    els.attachmentsList = document.getElementById('attachments-list');
    els.composeQuote = document.getElementById('compose-quote');
    els.composeAttachments = document.getElementById('compose-attachments');
    els.composeAttachmentsList = document.getElementById('compose-attachments-list');
    els.composeFileInput = document.getElementById('compose-file-input');
    els.starredItem = document.getElementById('starred-item');
    els.accountErrorBanner = document.getElementById('account-error-banner');
    els.accountErrorDetails = document.getElementById('account-error-details');
    // Timezone banner + settings
    els.tzChangeBanner = document.getElementById('tz-change-banner');
    els.tzChangeText = document.getElementById('tz-change-text');
    els.tzAcceptSystem = document.getElementById('tz-accept-system');
    els.tzKeepCurrent = document.getElementById('tz-keep-current');
    els.tzRecheck = document.getElementById('tz-recheck');
    els.tzDetected = document.getElementById('tz-detected');
    els.tzModeSystem = document.getElementById('tz-mode-system');
    els.tzModeManual = document.getElementById('tz-mode-manual');
    els.tzManualPrimary = document.getElementById('tz-manual-primary');
    els.tzAdditionalChips = document.getElementById('tz-additional-chips');
    els.tzAdditionalInput = document.getElementById('tz-additional-input');
    els.tzAdditionalAdd = document.getElementById('tz-additional-add');
    els.tzSave = document.getElementById('tz-save');
    els.tzSaveStatus = document.getElementById('tz-save-status');
    els.tzIanaList = document.getElementById('tz-iana-list');
    // Compose-invite
    els.composeInviteEnabled = document.getElementById('compose-invite-enabled');
    els.composeInviteFields = document.getElementById('compose-invite-fields');
    els.inviteSummary = document.getElementById('invite-summary');
    els.inviteLocation = document.getElementById('invite-location');
    els.inviteStart = document.getElementById('invite-start');
    els.inviteEnd = document.getElementById('invite-end');
    els.inviteTz = document.getElementById('invite-tz');
    // Settings view
    els.settingsView = document.getElementById('settings-view');
    els.accountPaneList = document.getElementById('account-pane-list');
    els.accountEmpty = document.getElementById('account-empty');
    els.accountForm = document.getElementById('account-form');
    els.acctProvider = document.getElementById('acct-provider');
    els.acctName = document.getElementById('acct-name');
    els.acctUsername = document.getElementById('acct-username');
    els.acctEmail = document.getElementById('acct-email');
    els.acctApiToken = document.getElementById('acct-api-token');
    els.acctClientId = document.getElementById('acct-client-id');
    els.acctClientSecret = document.getElementById('acct-client-secret');
    els.acctAuthPill = document.getElementById('acct-auth-pill');
    els.acctAuthorizeBtn = document.getElementById('acct-authorize-btn');
    els.acctDefaultMarker = document.getElementById('acct-default-marker');
    els.acctSetDefault = document.getElementById('acct-set-default');
    els.acctSave = document.getElementById('acct-save');
    els.acctDelete = document.getElementById('acct-delete');
    els.acctConfirmDelete = document.getElementById('acct-confirm-delete');
    els.acctFormError = document.getElementById('acct-form-error');
    // Event listeners
    if (els.starredItem) {
        els.starredItem.addEventListener('click', toggleStarredOnly);
        els.starredItem.addEventListener('keydown', (e) => {
            if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                toggleStarredOnly();
            }
        });
    }
    els.accountErrorBanner.querySelector('.error-banner-dismiss').addEventListener('click', () => {
        els.accountErrorBanner.classList.add('hidden');
    });
    document.addEventListener('keydown', handleKeyDown);
    els.commandInput.addEventListener('input', handleCommandInput);
    els.searchInput.addEventListener('keydown', handleSearchKeyDown);
    els.searchInput.addEventListener('input', handleSearchInputChange);
    els.searchTokens.addEventListener('click', (e) => {
        const btn = e.target.closest('.chip-remove');
        if (!btn) return;
        const idx = parseInt(btn.dataset.index);
        state.searchTokens.splice(idx, 1);
        renderSearchChips();
        els.searchInput.focus();
    });
    els.activeFilterChips.addEventListener('click', (e) => {
        const btn = e.target.closest('.chip-remove');
        if (!btn) return;
        const idx = parseInt(btn.dataset.index);
        state.searchTokens.splice(idx, 1);
        updateActiveFilters();
        loadEmails();
    });
    els.clearAllFilters.addEventListener('click', clearAllFilters);
    els.undoButton.addEventListener('click', performUndo);
    els.splitCancel.addEventListener('click', closeSplitModal);
    els.splitSave.addEventListener('click', saveSplit);
    els.splitFilterType.addEventListener('change', updateSplitModalFields);
    els.rsvpAccept.addEventListener('click', () => rsvpToEvent('ACCEPTED'));
    els.rsvpMaybe.addEventListener('click', () => rsvpToEvent('TENTATIVE'));
    els.rsvpDecline.addEventListener('click', () => rsvpToEvent('DECLINED'));
    els.composeFileInput.addEventListener('change', handleFileSelect);
    els.composeAttachmentsList.addEventListener('click', handleAttachmentListClick);
    setupComposeDragDrop();
    els.composeBody.addEventListener('paste', handleComposePaste);

    // Single delegated click handler for email list — never re-bound, survives innerHTML updates
    els.emailList.addEventListener('click', (e) => {
        const row = e.target.closest('.email-row');
        if (!row) return;
        state.selectedIndex = parseInt(row.dataset.index);
        renderEmailList();
        loadEmailDetail(row.dataset.id);
    });

    // Compose field listeners
    [els.composeTo, els.composeCc, els.composeSubject, els.composeBody].forEach(el => {
        el.addEventListener('focus', () => setMode('insert'));
        el.addEventListener('blur', () => setMode('normal'));
    });

    // Auto-expand textarea as user types
    els.composeBody.addEventListener('input', autoResizeTextarea);

    function autoResizeTextarea() {
        els.composeBody.style.height = 'auto';
        els.composeBody.style.height = els.composeBody.scrollHeight + 'px';
    }

    // Settings event listeners
    els.acctProvider.addEventListener('change', updateProviderFields);
    els.accountForm.addEventListener('submit', (e) => {
        e.preventDefault();
        saveAccount();
    });
    els.acctAuthorizeBtn.addEventListener('click', () => {
        // The dense form is only reachable for existing accounts now —
        // new accounts go through the wizard, which owns its own
        // save→authorize flow. selectedAccountId is always set here.
        if (state.selectedAccountId) authorize(state.selectedAccountId);
    });
    els.acctSetDefault.addEventListener('click', () => {
        if (state.selectedAccountId) setDefaultAccount(state.selectedAccountId);
    });
    els.acctDelete.addEventListener('click', toggleConfirmDelete);
    els.acctConfirmDelete.addEventListener('click', (e) => {
        const btn = e.target.closest('button[data-confirm]');
        if (!btn) return;
        if (btn.dataset.confirm === 'yes') actuallyDeleteAccount();
        else els.acctConfirmDelete.classList.add('hidden');
    });
    els.accountForm.addEventListener('click', (e) => {
        const btn = e.target.closest('.reveal-btn');
        if (!btn) return;
        const target = document.getElementById(btn.dataset.target);
        if (!target) return;
        const showing = target.type === 'text';
        target.type = showing ? 'password' : 'text';
        btn.classList.toggle('active', !showing);
        btn.textContent = showing ? 'reveal' : 'hide';
    });
    els.accountPaneList.addEventListener('click', (e) => {
        const row = e.target.closest('.account-row[data-id]');
        if (!row) return;
        state.selectedAccountId = row.dataset.id;
        state.settingsMode = 'edit';
        renderSettings();
    });
    document.querySelector('#settings-view .add-row').addEventListener('click', () => {
        beginAddAccount();
    });

    // Wizard event listeners
    document.querySelectorAll('#wiz-picker .wiz-row').forEach((row, i) => {
        row.addEventListener('mouseenter', () => { if (state.wizardStep === 1) focusWizProvider(i); });
        row.addEventListener('click', () => {
            focusWizProvider(i);
            wizGoTo(2);
        });
    });
    // Only text-like inputs should flip global mode to insert — the step-4
    // "Set as default" checkbox stays in normal mode so the wizard's NORMAL
    // pill remains accurate.
    document.querySelectorAll('#wiz input[type=text], #wiz input[type=email], #wiz input[type=password], #wiz select').forEach(el => {
        el.addEventListener('focus', () => { if (state.wizardActive) setMode('insert'); });
        el.addEventListener('blur', () => { if (state.wizardActive) setMode('normal'); });
    });
    document.querySelectorAll('#wiz .wiz-reveal').forEach(btn => {
        btn.addEventListener('click', () => {
            const target = document.getElementById(btn.dataset.wizReveal);
            if (!target) return;
            const showing = target.type === 'text';
            target.type = showing ? 'password' : 'text';
            btn.textContent = showing ? 'show' : 'hide';
        });
    });
    document.getElementById('wiz-form').addEventListener('submit', (e) => {
        e.preventDefault();
        wizContinueFromCreds();
    });
    // Cache typed values per provider so esc-back/reopen preserves them.
    const wizFieldMap = {
        'wiz-name':          'name',
        'wiz-client-id':     'client-id',
        'wiz-client-secret': 'client-secret',
        'wiz-username':      'username',
        'wiz-api-token':     'api-token',
    };
    Object.keys(wizFieldMap).forEach(id => {
        const el = document.getElementById(id);
        if (!el) return;
        el.addEventListener('input', () => {
            if (!state.wizardActive) return;
            const provider = WIZ_PROVIDERS[state.wizardProviderIdx];
            state.wizardCache[provider][wizFieldMap[id]] = el.value;
            if (id === 'wiz-name') {
                state.wizardCache[provider].nameTouched = true;
                checkWizOverwrite();
            }
            if (id === 'wiz-client-secret' || id === 'wiz-api-token') updateWizCachedHints();
        });
    });
    document.querySelectorAll('#wiz [data-wiz-action]').forEach(btn => {
        btn.addEventListener('click', () => {
            switch (btn.dataset.wizAction) {
                case 'back-to-1':         wizGoTo(1); break;
                case 'cancel-connecting': wizCancelConnecting(); break;
                case 'add-another':       wizGoTo(1); break;
                case 'finish':            wizFinish(); break;
            }
        });
    });
    // Reload theme on window focus (pick up theme changes after alt-tabbing back)
    window.addEventListener('focus', loadTheme);

    // Timezone listeners
    els.tzAcceptSystem.addEventListener('click', acceptSystemTimezone);
    els.tzKeepCurrent.addEventListener('click', dismissTimezoneChange);
    els.tzRecheck.addEventListener('click', loadTimezone);
    els.tzModeSystem.addEventListener('change', renderTimezoneSettings);
    els.tzModeManual.addEventListener('change', renderTimezoneSettings);
    els.tzAdditionalAdd.addEventListener('click', addAdditionalTz);
    els.tzAdditionalInput.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') { e.preventDefault(); addAdditionalTz(); }
    });
    els.tzSave.addEventListener('click', saveTimezoneSettings);

    // Compose-invite toggle
    els.composeInviteEnabled.addEventListener('change', () => {
        els.composeInviteFields.classList.toggle('hidden', !els.composeInviteEnabled.checked);
        if (els.composeInviteEnabled.checked && !els.inviteTz.value && state.timezone) {
            els.inviteTz.value = state.timezone.primary;
        }
    });

    // Load data
    loadTheme();
    loadAccounts();
    loadSplits();
    loadTimezone();
    loadTzZones();
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

        // Determine light mode: Omarchy theme takes precedence, otherwise follow OS
        const isLight = css.trim()
            ? css.includes('--light-mode')
            : window.matchMedia('(prefers-color-scheme: light)').matches;
        document.body.classList.toggle('light-theme', isLight);
    } catch (err) {
        console.warn('Failed to load theme:', err);
    }
}

// Live-update when macOS appearance changes
window.matchMedia('(prefers-color-scheme: light)').addEventListener('change', loadTheme);

// Timezone

async function loadTimezone() {
    try {
        const tz = await fetch('/api/timezone').then(r => r.json());
        state.timezone = tz;
        renderTzBanner();
        renderTimezoneSettings();
        // Refresh the calendar card if currently visible.
        if (state.currentEmail?.calendarEvent) {
            renderCalendarCard(state.currentEmail.calendarEvent);
        }
    } catch (err) {
        console.warn('Failed to load timezone settings:', err);
    }
}

async function loadTzZones() {
    try {
        const zones = await fetch('/api/timezone/zones').then(r => r.json());
        state.tzZones = zones;
        els.tzIanaList.innerHTML = zones
            .map(z => `<option value="${escapeHtml(z)}">`).join('');
    } catch (err) {
        console.warn('Failed to load tz zone list:', err);
    }
}

function renderTzBanner() {
    if (!state.timezone) return;
    if (state.timezone.system_changed) {
        els.tzChangeText.textContent =
            `System timezone changed to ${state.timezone.system}. Current primary: ${state.timezone.primary}.`;
        els.tzChangeBanner.classList.remove('hidden');
    } else {
        els.tzChangeBanner.classList.add('hidden');
    }
}

function renderTimezoneSettings() {
    if (!state.timezone || !els.tzDetected) return;
    els.tzDetected.textContent = state.timezone.system;

    // Mode radios: respect the manual radio if the user just clicked it
    // (the user may be configuring before saving), otherwise reflect persisted state.
    const userPicking = document.activeElement === els.tzModeManual ||
                        document.activeElement === els.tzModeSystem;
    if (!userPicking) {
        els.tzModeSystem.checked = state.timezone.use_system;
        els.tzModeManual.checked = !state.timezone.use_system;
    }
    const manual = els.tzModeManual.checked;
    els.tzManualPrimary.disabled = !manual;
    if (!els.tzManualPrimary.value && !state.timezone.use_system) {
        els.tzManualPrimary.value = state.timezone.primary;
    }

    // Additional TZ chips: derived from state.timezone.display minus primary
    const additional = (state.timezone.display || [])
        .filter(tz => tz !== state.timezone.primary);
    els.tzAdditionalChips.innerHTML = additional.map(tz => `
        <span class="tz-chip" data-tz="${escapeHtml(tz)}">
            ${escapeHtml(tz)}
            <button type="button" class="tz-chip-remove" data-tz="${escapeHtml(tz)}">&times;</button>
        </span>
    `).join('');
    els.tzAdditionalChips.querySelectorAll('.tz-chip-remove').forEach(btn => {
        btn.addEventListener('click', () => {
            const tz = btn.dataset.tz;
            removeAdditionalTzFromState(tz);
        });
    });
}

function getAdditionalTzList() {
    return Array.from(els.tzAdditionalChips.querySelectorAll('.tz-chip'))
        .map(el => el.dataset.tz);
}

function addAdditionalTz() {
    const tz = els.tzAdditionalInput.value.trim();
    if (!tz) return;
    if (state.tzZones.length && !state.tzZones.includes(tz)) {
        els.tzSaveStatus.textContent = `Unknown timezone: ${tz}`;
        els.tzSaveStatus.className = 'tz-save-status error';
        els.tzSaveStatus.classList.remove('hidden');
        return;
    }
    if (getAdditionalTzList().includes(tz)) {
        els.tzAdditionalInput.value = '';
        return;
    }
    const chip = document.createElement('span');
    chip.className = 'tz-chip';
    chip.dataset.tz = tz;
    chip.innerHTML = `${escapeHtml(tz)}
        <button type="button" class="tz-chip-remove" data-tz="${escapeHtml(tz)}">&times;</button>`;
    chip.querySelector('.tz-chip-remove').addEventListener('click', () => chip.remove());
    els.tzAdditionalChips.appendChild(chip);
    els.tzAdditionalInput.value = '';
    els.tzSaveStatus.classList.add('hidden');
}

function removeAdditionalTzFromState(tz) {
    const chip = els.tzAdditionalChips.querySelector(`.tz-chip[data-tz="${CSS.escape(tz)}"]`);
    if (chip) chip.remove();
}

async function saveTimezoneSettings() {
    const body = {
        use_system: els.tzModeSystem.checked,
        manual_primary: els.tzManualPrimary.value.trim() || null,
        additional: getAdditionalTzList(),
    };
    try {
        const resp = await fetch('/api/timezone', {
            method: 'PUT',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(body),
        });
        if (!resp.ok) throw new Error(await resp.text());
        state.timezone = await resp.json();
        els.tzSaveStatus.textContent = 'Saved.';
        els.tzSaveStatus.className = 'tz-save-status ok';
        els.tzSaveStatus.classList.remove('hidden');
        setTimeout(() => els.tzSaveStatus.classList.add('hidden'), 2000);
        renderTzBanner();
        renderTimezoneSettings();
        // Re-render the visible calendar card so the new display TZs take effect.
        if (state.currentEmail?.calendarEvent) {
            renderCalendarCard(state.currentEmail.calendarEvent);
        }
    } catch (err) {
        els.tzSaveStatus.textContent = `Save failed: ${err.message}`;
        els.tzSaveStatus.className = 'tz-save-status error';
        els.tzSaveStatus.classList.remove('hidden');
    }
}

async function acceptSystemTimezone() {
    try {
        const resp = await fetch('/api/timezone/accept-system', { method: 'POST' });
        if (!resp.ok) throw new Error(await resp.text());
        state.timezone = await resp.json();
        renderTzBanner();
        renderTimezoneSettings();
        if (state.currentEmail?.calendarEvent) {
            renderCalendarCard(state.currentEmail.calendarEvent);
        }
    } catch (err) {
        showStatus('Failed to update timezone: ' + err.message, 'error');
    }
}

async function dismissTimezoneChange() {
    try {
        // Send the system TZ value the user was looking at so the server can
        // refuse if the system TZ moved between banner-display and click.
        const seen_system = state.timezone?.system || null;
        const resp = await fetch('/api/timezone/dismiss-change', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ seen_system }),
        });
        if (!resp.ok) {
            // 409 Conflict: the system TZ changed again; refresh the banner.
            if (resp.status === 409) {
                await loadTimezone();
                showStatus('System timezone changed again — please review the banner.', 'error');
                return;
            }
            throw new Error(await resp.text());
        }
        state.timezone = await resp.json();
        renderTzBanner();
    } catch (err) {
        showStatus('Failed to dismiss: ' + err.message, 'error');
    }
}

// API calls

async function api(method, path, body = null, signal = null) {
    const opts = {
        method,
        headers: { 'Content-Type': 'application/json' },
    };
    if (body) opts.body = JSON.stringify(body);
    if (signal) opts.signal = signal;

    // Auto-append ?account= ONLY for account-scoped routes. Settings routes
    // (`/accounts/...`, `/theme`) are global and must never be tagged.
    let url = '/api' + path;
    if (state.currentAccount && ACCOUNT_SCOPED_API.test(path)) {
        const separator = url.includes('?') ? '&' : '?';
        url += `${separator}account=${state.currentAccount.id}`;
    }

    const resp = await fetch(url, opts);
    if (!resp.ok) {
        const err = await resp.text();
        throw new Error(err);
    }
    if (resp.status === 204) return null;
    const text = await resp.text();
    return text ? JSON.parse(text) : null;
}

async function loadAccounts() {
    try {
        const data = await fetch('/api/accounts').then(r => r.json());
        state.accounts = data.accounts;
        renderAccounts();

        const nonSetupErrors = (data.errors || []).filter(e => e.provider !== 'setup');
        if (nonSetupErrors.length > 0) {
            showAccountErrors(nonSetupErrors);
        } else {
            els.accountErrorBanner.classList.add('hidden');
        }

        // First-run: no accounts at all → land directly in settings.
        if (!state.accounts.length) {
            state.currentAccount = null;
            state.currentMailbox = null;
            state.emails = [];
            els.mailboxName.textContent = 'NO ACCOUNTS';
            openSettings({ firstRun: true });
            return;
        }

        const defaultAcc = state.accounts.find(a => a.isDefault) || state.accounts[0];
        if (defaultAcc) selectAccount(defaultAcc);

        // If we were already in settings (e.g. just completed first-run save),
        // re-render to show the new account list rather than the firstRun pane.
        if (state.view === 'settings') renderSettings();
    } catch (err) {
        showStatus('Failed to load accounts: ' + err.message, 'error');
    }
}

function showAccountErrors(errors) {
    const count = errors.length;
    const list = errors.map(e => {
        const acctText = escapeHtml(e.account);
        const acctAttr = escapeAttr(e.account);
        const prov = escapeHtml(e.provider);
        const body = escapeHtml(e.error);
        // The Authorize button is purely structural — gated on authStatus,
        // independent of error text. The backend can reword "Not authorized
        // — click Authorize" however it wants and the button still appears.
        const acctRec = state.accounts.find(a => a.id === e.account);
        const needsAuth = acctRec && acctRec.authStatus === 'pending';
        const action = needsAuth
            ? ` <button type="button" class="banner-authorize-link" data-account-id="${acctAttr}">[ Authorize ]</button>`
            : '';
        return `<li><strong>${acctText}</strong> (${prov}): ${body}${action}</li>`;
    }).join('');
    els.accountErrorDetails.innerHTML =
        `<strong>${count} account${count > 1 ? 's' : ''} failed to connect:</strong><ul>${list}</ul>`;
    els.accountErrorBanner.classList.remove('hidden');
    els.accountErrorDetails.querySelectorAll('.banner-authorize-link').forEach(btn => {
        btn.addEventListener('click', () => authorizeAccountFromBanner(btn.dataset.accountId));
    });
}

async function authorizeAccountFromBanner(id) {
    // Banner state can be stale (account removed, or it just succeeded
    // somewhere else). Refresh and re-check before kicking off the flow.
    await loadAccounts();
    const acct = state.accounts.find(a => a.id === id);
    if (!acct) {
        showStatus(`Account ${id} no longer exists`, 'error');
        return;
    }
    if (acct.authStatus !== 'pending') {
        showStatus(`${id} is already authorized`, 'info');
        return;
    }
    state.selectedAccountId = id;
    state.settingsMode = 'edit';
    showView('settings');
    renderSettings();
    showStatus(`Authorizing ${id}…`, 'info');
    authorize(id);
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
    state.currentEmail = null;
    state.selectedIndex = 0;
    state.currentSplit = 'all';
    clearSplitListCache();
    // emailCache and scrollPositions are keyed by previous-account email ids;
    // leaving them populated lets prefetch/RSVP/keyboard-nav fire a stale id
    // against the new account's backend and surface as a 400 from the wrong
    // provider. const-bound, so wipe in place.
    for (const k in emailCache) delete emailCache[k];
    for (const k in scrollPositions) delete scrollPositions[k];
    renderAccounts();
    loadMailboxes();
    loadIdentities();
}

async function loadSplits() {
    try {
        state.splits = await fetch('/api/splits').then(r => r.json());
        renderSplitTabs();
        loadSplitCounts();
    } catch (err) {
        console.warn('Failed to load splits:', err);
        state.splits = [];
    }
}

let splitCountsController = null;

async function loadSplitCounts() {
    if (state.currentMailbox?.role !== 'inbox' || state.splits.length === 0) return;
    if (splitCountsController) splitCountsController.abort();
    splitCountsController = new AbortController();
    const mailboxId = state.currentMailbox.id;
    try {
        let url = `/split-counts?mailbox_id=${mailboxId}`;
        if (state.starredOnly) url += '&starred=true';
        const counts = await api('GET', url, null, splitCountsController.signal);
        if (state.currentMailbox?.id !== mailboxId) return; // stale response guard
        state.splitCounts = counts;
        renderSplitTabs();
    } catch (err) {
        if (err.name !== 'AbortError') console.warn('Failed to load split counts:', err);
    } finally {
        splitCountsController = null;
    }
}

function adjustSplitCounts(delta) {
    if (state.splitCounts.all != null) {
        const next = state.splitCounts.all + delta;
        if (next < 0) console.warn('split count underflow: all', state.splitCounts.all, delta);
        state.splitCounts.all = Math.max(0, next);
    }
    if (state.currentSplit && state.currentSplit !== 'all' && state.splitCounts[state.currentSplit] != null) {
        const next = state.splitCounts[state.currentSplit] + delta;
        if (next < 0) console.warn('split count underflow:', state.currentSplit, state.splitCounts[state.currentSplit], delta);
        state.splitCounts[state.currentSplit] = Math.max(0, next);
    }
    renderSplitTabs();
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
        `<option value="${id.email}">${id.email}${id.name ? ' (' + id.name + ')' : ''}</option>`
    ).join('');
}

function getSplitIcon(split) {
    if (!split.icon) return '';
    return `<img class="split-icon" src="${escapeHtml(split.icon)}" width="14" height="14" alt="" onerror="this.style.display='none'">`;
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

    els.splitTabs.innerHTML = tabs.map((split, idx) => {
        const count = state.splitCounts[split.id];
        const countBadge = count != null ? `<span class="split-count">${escapeHtml(String(count))}</span>` : '';
        return `
        <div class="split-tab ${state.currentSplit === split.id ? 'active' : ''}"
             data-split="${split.id}" title="Ctrl+${idx + 1}">
            <span class="split-name">${getSplitIcon(split)}${escapeHtml(split.name)}</span>${countBadge}
        </div>
    `;
    }).join('');

    els.splitTabs.querySelectorAll('.split-tab').forEach(el => {
        el.addEventListener('click', () => selectSplit(el.dataset.split));
    });
}

function splitCacheKey() {
    return `${state.currentAccount?.id || ''}:${state.currentMailbox?.id || ''}:${state.currentSplit || 'all'}:${state.starredOnly ? 'S' : ''}:${getSearchQuery()}`;
}

function clearSplitListCache() {
    Object.keys(splitListCache).forEach(k => delete splitListCache[k]);
}

function invalidateSplitListCache() {
    delete splitListCache[splitCacheKey()];
}

function selectSplit(splitId) {
    state.currentSplit = splitId;
    renderSplitTabs();

    // Show cached split data instantly — no network wait
    const key = splitCacheKey();
    if (splitListCache[key]) {
        state.emails = [...splitListCache[key]];
        state.selectedIndex = 0;
        renderEmailList();
    }

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

function buildEmailListUrl(mailboxId, { offset = 0 } = {}) {
    let url = `/emails?mailbox_id=${mailboxId}&limit=${CACHE_LIMIT}`;
    if (offset > 0) url += `&offset=${offset}`;
    if (state.currentMailbox?.role === 'inbox' && state.currentSplit && state.currentSplit !== 'all' && state.splits.length > 0) {
        url += `&split_id=${state.currentSplit}`;
    }
    if (state.starredOnly) url += `&starred=true`;
    const search = getSearchQuery();
    if (search) url += `&search=${encodeURIComponent(search)}`;
    return url;
}

async function loadEmails() {
    if (!state.currentMailbox) return;

    // Cancel any in-flight email fetch
    if (loadEmailsController) loadEmailsController.abort();
    loadEmailsController = new AbortController();

    // Snapshot context at request time for stale detection
    const context = splitCacheKey();

    // Show loading only if we have no data (cache miss)
    if (state.emails.length === 0) {
        els.emailList.innerHTML = '<div class="loading">Loading</div>';
    }

    try {
        const url = buildEmailListUrl(state.currentMailbox.id);
        const emails = await api('GET', url, null, loadEmailsController.signal);

        // Stale response guard: discard if context changed during fetch
        if (splitCacheKey() !== context) return;

        splitListCache[context] = [...emails];
        state.emails = emails;
        state.selectedIndex = 0;
        renderEmailList();
    } catch (err) {
        if (err.name !== 'AbortError') {
            showStatus('Failed to load emails: ' + err.message, 'error');
        }
    }
}

async function maybeRefillEmails() {
    if (refillInFlight || state.emails.length >= REFILL_THRESHOLD) return;
    if (!state.currentMailbox) return;

    const context = splitCacheKey();

    refillInFlight = true;
    try {
        const url = buildEmailListUrl(state.currentMailbox.id, { offset: state.emails.length });
        const fresh = await api('GET', url);

        // Discard results if context changed during fetch (mailbox, split, or search)
        if (splitCacheKey() !== context) return;

        const existingIds = new Set(state.emails.map(e => e.id));
        const newEmails = fresh.filter(e => !existingIds.has(e.id));
        if (newEmails.length > 0) {
            state.emails = state.emails.concat(newEmails);
            splitListCache[context] = [...state.emails];
            renderEmailList();
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

    // Use cache if available — render immediately, no await
    if (emailCache[emailId]) {
        state.currentEmail = emailCache[emailId];
        renderEmailDetail();
        els.emailBody.scrollTop = scrollPositions[emailId] || 0;
        showView('detail');
        prefetchAdjacentEmails();
        return;
    }

    // Not cached: show partial data from list immediately so the UI never feels stuck.
    // The list item has subject, from, date — render that now, fetch body in background.
    const listItem = state.emails.find(e => e.id === emailId);
    if (listItem) {
        state.currentEmail = listItem;
        renderEmailDetailPartial(listItem);
        showView('detail');
    } else {
        els.calendarEvent.classList.add('hidden');
    }

    try {
        const email = await api('GET', `/emails/${emailId}`);
        emailCache[emailId] = email;
        // Only render if we're still looking at this email (user may have navigated away)
        if (state.currentEmail?.id === emailId) {
            state.currentEmail = email;
            renderEmailDetail();
            els.emailBody.scrollTop = 0;
        }
        showView('detail');
        prefetchAdjacentEmails();
    } catch (err) {
        showStatus('Failed to load email: ' + err.message, 'error');
    }
}

// Render what we know from list data: subject, from, date. Clear body.
// This gives instant visual feedback while the full email loads.
function renderEmailDetailPartial(listItem) {
    const from = listItem.from[0];
    const fromDisplay = from?.name ? `${from.name} <${from.email}>` : from?.email || 'Unknown';
    const toDisplay = listItem.to ? listItem.to.map(t => t.name || t.email).join(', ') : '';
    const date = new Date(listItem.receivedAt).toLocaleString();

    els.emailSubject.textContent = listItem.subject;
    els.emailMeta.innerHTML = `
        <div><span class="label">From:</span> ${escapeHtml(fromDisplay)}</div>
        ${toDisplay ? `<div><span class="label">To:</span> ${escapeHtml(toDisplay)}</div>` : ''}
        <div><span class="label">Date:</span> ${date}</div>
    `;
    els.calendarEvent.classList.add('hidden');
    els.attachments.classList.add('hidden');
    els.emailBody.innerHTML = '<div class="loading-body">Loading…</div>';
    els.emailBody.classList.remove('html-content');
}

// Prefetch next few emails so archive/navigation is instant.
// Fire-and-forget — no awaits, no blocking the UI.
function prefetchAdjacentEmails() {
    const idx = state.emails.findIndex(e => e.id === state.currentEmail?.id);
    if (idx < 0) return;

    // Prefetch next 3 emails (the ones you'll hit when archiving repeatedly)
    for (let i = 1; i <= 3; i++) {
        const target = state.emails[idx + i];
        if (target && !emailCache[target.id]) {
            api('GET', `/emails/${target.id}`)
                .then(email => { emailCache[target.id] = email; })
                .catch(() => {}); // Swallow — prefetch is best-effort
        }
    }
}

async function emailAction(type, emailId) {
    const label = type === 'archive' ? 'Archived' : 'Trashed';

    // Optimistic: remove from list and show feedback immediately
    const removedEmail = state.emails.find(e => e.id === emailId);
    const removedIndex = state.emails.indexOf(removedEmail);
    pushUndo(label.toLowerCase(), emailId, removedEmail, removedIndex);
    removeEmailFromList(emailId);
    showStatus(label, 'success');

    try {
        await api('POST', `/emails/${emailId}/${type}`);
        loadSplitCounts(); // resync with server truth
    } catch (err) {
        // Revert: re-insert the email and remove the stale undo entry
        state.undoStack.pop();
        if (removedEmail) {
            state.emails.splice(removedIndex, 0, removedEmail);
            invalidateSplitListCache();
            renderEmailList();
        }
        adjustSplitCounts(+1);
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
    const userText = els.composeBody.value;

    if (!to.length) {
        showStatus('No recipients', 'error');
        return;
    }

    if (state.pendingAttachments.some(a => a.status === 'uploading')) {
        showStatus('Wait for uploads to finish', 'error');
        return;
    }

    const quotedText = state.replyContext?.quotedText;
    const quotedHtml = state.replyContext?.quotedHtml;

    const fullTextBody = quotedText
        ? userText + '\n\n' + quotedText.split('\n').map(l => '> ' + l).join('\n')
        : userText;

    const fullHtmlBody = quotedHtml
        ? `<div>${escapeHtml(userText).replace(/\n/g, '<br>')}</div>`
          + `<blockquote style="border-left:2px solid #ccc;padding-left:12px;margin-left:0">${quotedHtml}</blockquote>`
        : null;

    const readyAttachments = state.pendingAttachments
        .filter(a => a.status === 'ready')
        .map(a => ({ blob_id: a.blob_id, name: a.name, mime_type: a.mime_type, size: a.size }));

    const includeInvite = els.composeInviteEnabled && els.composeInviteEnabled.checked;
    if (includeInvite) {
        const summary = els.inviteSummary.value.trim();
        const start = els.inviteStart.value;
        const end = els.inviteEnd.value;
        if (!summary || !start || !end) {
            showStatus('Invite needs title, start, and end', 'error');
            return;
        }
        const tz = (els.inviteTz.value.trim() || state.timezone?.primary || '').trim();
        const inviteAttendees = to.concat(cc).map(email => ({ email }));
        try {
            await api('POST', '/calendar/invite', {
                to,
                cc,
                subject,
                body: fullTextBody,
                summary,
                location: els.inviteLocation.value.trim() || null,
                description: null,
                start,
                end,
                tz: tz || null,
                attendees: inviteAttendees,
                from_address: fromAddress,
                // Roborev 186 #6: pass through attachments so the invite+files
                // combo doesn't silently drop the user's uploads.
                attachments: readyAttachments.length ? readyAttachments : undefined,
            });
            showStatus('Invite sent!', 'success');
            clearCompose();
            showView('list');
        } catch (err) {
            showStatus('Invite send failed: ' + err.message, 'error');
        }
        return;
    }

    try {
        await api('POST', '/emails/send', {
            to,
            cc,
            subject,
            body: fullTextBody,
            html_body: fullHtmlBody || undefined,
            in_reply_to: state.replyContext?.inReplyTo || null,
            from_address: fromAddress,
            attachments: readyAttachments.length ? readyAttachments : undefined,
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

    renderStarredItem();
}

function renderStarredItem() {
    if (!els.starredItem) return;
    els.starredItem.classList.toggle('active', state.starredOnly);
    els.starredItem.setAttribute('aria-pressed', String(state.starredOnly));
}

function toggleStarredOnly() {
    if (!state.currentMailbox) return;
    state.starredOnly = !state.starredOnly;
    clearSplitListCache();
    renderStarredItem();
    updateMailboxNameDisplay();
    loadEmails();
    if (state.currentMailbox.role === 'inbox') loadSplitCounts();
}

function updateMailboxNameDisplay() {
    if (!state.currentMailbox) return;
    const base = state.currentMailbox.name.toUpperCase();
    els.mailboxName.textContent = state.starredOnly ? `${base} · STARRED` : base;
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

    // Render attachments if present
    if (e.attachments?.length) {
        renderAttachments(e.attachments, e.id);
    } else {
        els.attachments.classList.add('hidden');
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
    els.settingsView.classList.toggle('active', view === 'settings');

    if (view === 'compose') {
        els.composeTo.focus();
    }
}

function selectMailbox(mailbox) {
    state.currentMailbox = mailbox;
    state.searchTokens = [];
    state.currentSplit = mailbox.role === 'inbox' ? 'all' : null;
    state.splitCounts = {};
    clearSplitListCache();
    updateMailboxNameDisplay();
    renderMailboxes();
    renderSplitTabs();
    updateActiveFilters();
    loadEmails();
    if (mailbox.role === 'inbox') loadSplitCounts();
}

function setMode(mode) {
    state.mode = mode;
    els.modeIndicator.textContent = mode === 'awaiting' ? '-- AWAITING AUTHORIZATION --' : mode.toUpperCase();
    els.modeIndicator.className = mode;
}

// ============================================================================
// Settings view
// ============================================================================

function openSettings({ firstRun = false } = {}) {
    if (firstRun) {
        state.selectedAccountId = null;
        state.settingsMode = 'view';
    }
    showView('settings');
    renderSettings();
    if (firstRun) openWizard();
}

function closeSettings() {
    if (state.wizardActive) closeWizard();
    if (state.authController) {
        state.authController.abort();
        state.authController = null;
    }
    els.acctConfirmDelete.classList.add('hidden');
    els.acctFormError.classList.add('hidden');
    state.settingsMode = 'view';
    if (state.accounts.length === 0) return; // first-run: stay until they add one
    showView('list');
    setMode('normal');
}

function renderSettings() {
    // Master list
    els.accountPaneList.innerHTML = state.accounts.map((a, idx) => {
        const isSel = a.id === state.selectedAccountId;
        const star = a.isDefault ? '<span class="default-star">★</span>' : '';
        return `
            <div class="account-row ${isSel ? 'selected' : ''}" data-id="${escapeHtml(a.id)}">
                <span class="account-row-key">${idx + 1}</span>
                <span class="account-row-email">${star} ${escapeHtml(a.email || a.id)}</span>
                <span class="account-row-provider">${escapeHtml(a.provider)}</span>
            </div>`;
    }).join('');

    // Wizard takes precedence over the form/empty state for new accounts.
    const wiz = document.getElementById('wiz');
    if (state.wizardActive) {
        els.accountForm.classList.add('hidden');
        els.accountEmpty.classList.add('hidden');
        if (wiz) wiz.classList.remove('hidden');
        return;
    }
    if (wiz) wiz.classList.add('hidden');

    // Detail pane: empty/firstrun shell vs. edit form
    if (state.settingsMode === 'view' && !state.selectedAccountId) {
        els.accountForm.classList.add('hidden');
        els.accountEmpty.classList.remove('hidden');
        if (state.accounts.length === 0) {
            els.accountEmpty.innerHTML = `
                <h2>No accounts configured.</h2>
                <p>Press <kbd>a</kbd> or click <em>+ Add account</em> to set up your first one.</p>
                <p>Your config will be saved to <code>~/.config/supervillain/config</code>.</p>`;
        } else {
            els.accountEmpty.innerHTML = `
                <p>Select an account on the left, or press <kbd>a</kbd> to add a new one.</p>`;
        }
        return;
    }

    // Edit form
    els.accountEmpty.classList.add('hidden');
    els.accountForm.classList.remove('hidden');
    els.acctFormError.classList.add('hidden');

    const existing = state.accounts.find(a => a.id === state.selectedAccountId);
    const editingExisting = !!existing;

    // Mode flags
    els.accountForm.querySelectorAll('[data-when-editing]').forEach(el => {
        el.style.display = editingExisting ? '' : 'none';
    });

    // Provider + name (immutable for existing accounts; type = re-add otherwise)
    if (existing) {
        els.acctProvider.value = existing.provider;
        els.acctProvider.disabled = true;
        els.acctName.value = existing.id;
        els.acctName.disabled = true;
    } else {
        els.acctProvider.disabled = false;
        els.acctName.disabled = false;
    }

    // Populate fields
    if (existing) {
        els.acctEmail.value = existing.email || '';
        els.acctUsername.value = existing.email || '';
        // Secrets are never echoed: blank = preserve existing.
        els.acctApiToken.value = '';
        els.acctApiToken.placeholder = 'unchanged (leave blank to keep)';
        els.acctClientSecret.value = '';
        els.acctClientSecret.placeholder = 'unchanged (leave blank to keep)';
        // client-id is not a secret — backend returns it on the existing record.
        els.acctClientId.value = existing.clientId || '';
        els.acctClientId.placeholder = '';
        els.acctDefaultMarker.textContent = existing.isDefault ? 'yes ★' : 'no';
        const pending = existing.authStatus === 'pending';
        els.acctAuthPill.className = 'auth-status-pill ' + (pending ? 'failed' : 'authorized');
        els.acctAuthPill.textContent = pending ? 'NEEDS AUTH' : 'AUTHORIZED';
    } else {
        els.acctName.value = '';
        els.acctUsername.value = '';
        els.acctEmail.value = '';
        els.acctApiToken.value = '';
        els.acctApiToken.placeholder = 'fmu1-...';
        els.acctClientId.value = '';
        els.acctClientId.placeholder = '';
        els.acctClientSecret.value = '';
        els.acctClientSecret.placeholder = '';
        els.acctDefaultMarker.textContent = 'no';
        els.acctAuthPill.className = 'auth-status-pill idle';
        els.acctAuthPill.textContent = 'IDLE';
    }

    updateProviderFields();
}

function updateProviderFields() {
    const provider = els.acctProvider.value;
    els.accountForm.querySelectorAll('[data-provider]').forEach(el => {
        const providers = el.dataset.provider.split(',');
        el.style.display = providers.includes(provider) ? '' : 'none';
    });
}

function beginAddAccount() {
    // New accounts go through the 4-step wizard. Existing-account edits
    // continue to use the dense form.
    openWizard();
}

async function saveAccount() {
    const provider = els.acctProvider.value;
    let payload;
    if (provider === 'fastmail') {
        payload = {
            provider: 'fastmail',
            username: els.acctUsername.value.trim(),
            'api-token': els.acctApiToken.value, // empty → server preserves on update
        };
    } else if (provider === 'outlook') {
        payload = {
            provider: 'outlook',
            'client-id': els.acctClientId.value.trim(),
        };
    } else {
        payload = {
            provider: 'gmail',
            'client-id': els.acctClientId.value.trim(),
            'client-secret': els.acctClientSecret.value,
        };
    }
    const id = (els.acctName.value || state.selectedAccountId || '').trim();
    if (!id) {
        showFormError('Name is required');
        return;
    }
    try {
        const resp = await api('POST', `/accounts/${encodeURIComponent(id)}`, payload);
        showStatus(`Saved ${id}`, 'success');
        state.selectedAccountId = id;
        state.settingsMode = 'edit';
        await loadAccounts();
        setMode('normal');
        // OAuth providers need a second step.
        if (resp && resp.authStatus === 'pending') {
            showStatus(`Click [Authorize] to complete ${id} setup`, 'info');
        }
    } catch (err) {
        showFormError(err.message);
    }
}

function showFormError(msg) {
    els.acctFormError.textContent = msg;
    els.acctFormError.classList.remove('hidden');
}

function toggleConfirmDelete() {
    els.acctConfirmDelete.classList.toggle('hidden');
}

async function actuallyDeleteAccount() {
    if (!state.selectedAccountId) return;
    try {
        await api('DELETE', `/accounts/${encodeURIComponent(state.selectedAccountId)}`);
        showStatus(`Deleted ${state.selectedAccountId}`, 'success');
        state.selectedAccountId = null;
        state.settingsMode = 'view';
        state.currentEmail = null;
        state.emails = [];
        await loadAccounts();
    } catch (err) {
        showFormError(err.message);
    }
}

async function setDefaultAccount(id) {
    try {
        await api('PUT', `/accounts/${encodeURIComponent(id)}/default`);
        showStatus(`Default → ${id}`, 'success');
        await loadAccounts();
    } catch (err) {
        showFormError(err.message);
    }
}

async function authorize(id) {
    if (state.authController) state.authController.abort();
    state.authController = new AbortController();
    state.settingsMode = 'awaiting';
    setMode('awaiting');
    els.acctAuthPill.className = 'auth-status-pill awaiting';
    els.acctAuthPill.textContent = 'AWAITING';
    els.acctAuthorizeBtn.disabled = true;
    try {
        // Long-poll: server returns 200 when OAuth completes, 502 on failure.
        // The existing acquire_oauth_callback's 5-minute timeout caps the wait.
        await api('POST', `/accounts/${encodeURIComponent(id)}/authorize`,
            null, state.authController.signal);
        showStatus(`Authorized ${id}`, 'success');
        await loadAccounts();
    } catch (err) {
        if (err.name === 'AbortError') return;
        els.acctAuthPill.className = 'auth-status-pill failed';
        els.acctAuthPill.textContent = 'FAILED';
        showFormError(err.message);
    } finally {
        els.acctAuthorizeBtn.disabled = false;
        state.authController = null;
        state.settingsMode = 'edit';
        setMode('normal');
    }
}

// ============================================================================
// Add-account wizard (4 steps: pick provider → keys → connecting → done)
// ============================================================================

const WIZ_PROVIDERS = ['gmail', 'outlook', 'fastmail'];
const WIZ_CRUMBS = {
    1: '› choose provider',
    2: '› authorize',
    3: '› connecting',
    4: '› done',
};
const WIZ_HINTS = {
    1: '<kbd>1 2 3</kbd>pick &middot; <kbd>j k</kbd>move &middot; <kbd>enter</kbd>select &middot; <kbd>esc</kbd>cancel',
    2: '<kbd>tab</kbd>next field &middot; <kbd>S-tab</kbd>prev &middot; <kbd>enter</kbd>continue &middot; <kbd>esc</kbd>back',
    3: '<kbd>esc</kbd>cancel',
    4: '<kbd>enter</kbd>done &middot; <kbd>a</kbd>add another &middot; <kbd>esc</kbd>close',
};

function openWizard() {
    state.wizardActive = true;
    state.wizardStep = 1;
    state.wizardProviderIdx = 0;
    state.wizardSavedId = null;
    state.selectedAccountId = null;
    state.settingsMode = 'edit';
    renderSettings();
    renderWizardStep();
}

function closeWizard() {
    if (state.authController) {
        state.authController.abort();
        state.authController = null;
    }
    // Scrub cached secrets from JS memory on any wizard close (Esc, cancel
    // button, finish). Non-secret fields (name, client-id, username) stay so
    // a re-open after accidental close doesn't lose typed work; secrets are
    // cheap to re-paste and shouldn't linger keyed by provider.
    if (state.wizardCache) {
        Object.values(state.wizardCache).forEach(c => {
            c['client-secret'] = '';
            c['api-token']    = '';
        });
    }
    state.wizardActive = false;
    state.wizardSavedId = null;
    setMode('normal');
    renderSettings();
}

function wizGoTo(step) {
    state.wizardStep = step;
    renderWizardStep();
}

function renderWizardStep() {
    const n = state.wizardStep;
    document.querySelectorAll('.wiz-screen').forEach(s => {
        s.classList.toggle('visible', Number(s.dataset.wizStep) === n);
    });
    document.getElementById('wiz-step-now').textContent = String(n);
    document.getElementById('wiz-crumb').textContent = WIZ_CRUMBS[n];
    document.getElementById('wiz-hints').innerHTML = WIZ_HINTS[n];
    const modeEl = document.getElementById('wiz-mode');
    modeEl.textContent = n === 3 ? 'AWAITING' : 'NORMAL';
    modeEl.className = 'wiz-mode' + (n === 3 ? ' awaiting' : '');

    if (n === 1) {
        focusWizProvider(state.wizardProviderIdx);
        setMode('normal');
    } else if (n === 2) {
        tailorWizCreds();
    } else if (n === 4) {
        renderWizSuccess();
        setMode('normal');
        setTimeout(() => {
            const done = document.getElementById('wiz-done-btn');
            if (done) done.focus();
        }, 30);
    }
}

function focusWizProvider(idx) {
    const n = WIZ_PROVIDERS.length;
    state.wizardProviderIdx = ((idx % n) + n) % n;
    document.querySelectorAll('.wiz-row').forEach((r, i) => {
        r.classList.toggle('focused', i === state.wizardProviderIdx);
    });
}

function wizSuggestName(provider) {
    const taken = new Set(state.accounts.map(a => a.id));
    if (!taken.has(provider)) return provider;
    for (let n = 2; n < 1000; n++) {
        const cand = `${provider}-${n}`;
        if (!taken.has(cand)) return cand;
    }
    return `${provider}-${Date.now()}`;
}

// Provider descriptor table — single source of truth for everything that
// changes between providers. Adding a new provider is one entry here, plus
// the API-side support.
const WIZ_ALL_FIELDS = ['client-id', 'client-secret', 'username', 'api-token'];
const WIZ_FIELD_LABELS = {
    'client-id':     'Client ID',
    'client-secret': 'Client secret',
    'username':      'Email',
    'api-token':     'API token',
};
const WIZ_DESCRIPTORS = {
    gmail: {
        label: 'Google',
        title: 'Bring your own keys',
        blurb: `Supervillain talks to <em>Google</em> through an OAuth app <strong>you</strong> register &mdash; your inbox flows through your credentials, not ours.`,
        host: 'accounts.google.com',
        fields: ['client-id', 'client-secret'],
        placeholders: {
            'client-id':     '123…-abc.apps.googleusercontent.com',
            'client-secret': 'GOCSPX-…',
        },
        instructionsHtml: `
            <div class="wiz-why-head">Set up your Google OAuth client (~3&nbsp;min)</div>
            <ol class="wiz-steps">
                <li>Open <a href="https://console.cloud.google.com/apis/credentials" target="_blank" rel="noopener">Google Cloud &rarr; Credentials</a>. Create a project if you don&rsquo;t have one.</li>
                <li>Configure the <strong>OAuth consent screen</strong>: user type <strong>External</strong>; add yourself as a <strong>Test user</strong> under Audience (required while the app is in Testing mode &mdash; refresh tokens otherwise expire weekly).</li>
                <li>Enable APIs: <strong>Gmail API</strong> and <strong>Google Calendar API</strong> under Enabled APIs &amp; services.</li>
                <li><strong>+ Create Credentials &rarr; OAuth client ID</strong>. Application type: <strong>Desktop app</strong> (recommended &mdash; auto-allows loopback) or <strong>Web application</strong> with <code>http://127.0.0.1:8401/callback</code> registered as an authorized redirect URI.</li>
                <li>Copy the <strong>Client ID</strong> and <strong>Client Secret</strong> and paste them below.</li>
            </ol>`,
    },
    outlook: {
        label: 'Microsoft',
        title: 'Bring your own keys',
        blurb: `Supervillain talks to <em>Microsoft 365</em> through an OAuth app <strong>you</strong> register in Azure.`,
        host: 'login.microsoftonline.com',
        fields: ['client-id'],
        placeholders: { 'client-id': 'a1b2c3d4-...' },
        instructionsHtml: `
            <div class="wiz-why-head">Set up your Microsoft Entra app (~4&nbsp;min)</div>
            <ol class="wiz-steps">
                <li>Open <a href="https://entra.microsoft.com/" target="_blank" rel="noopener">Microsoft Entra &rarr; App registrations</a> and click <strong>New registration</strong>.</li>
                <li>Supported account types: <strong>Any organizational directory and personal Microsoft accounts</strong>.</li>
                <li>Redirect URI: <strong>Web</strong> &rarr; <code>http://localhost:8400/callback</code>.</li>
                <li>Under <strong>API permissions</strong>, add delegated: <strong>Mail.ReadWrite</strong>, <strong>Mail.Send</strong>, <strong>Calendars.ReadWrite</strong>.</li>
                <li>Copy the <strong>Application (client) ID</strong> and paste it below. No client secret needed &mdash; supervillain uses PKCE.</li>
            </ol>`,
    },
    fastmail: {
        label: 'Fastmail',
        title: 'Paste your Fastmail API token',
        blurb: `Fastmail doesn&rsquo;t use OAuth &mdash; you generate a scoped <em>JMAP + CalDAV</em> token in your Fastmail account settings.`,
        host: null,           // no browser/loopback step
        fields: ['username', 'api-token'],
        placeholders: { username: 'you@fastmail.com', 'api-token': 'fmu1-...' },
        instructionsHtml: `
            <div class="wiz-why-head">Get your Fastmail API token (~1&nbsp;min)</div>
            <ol class="wiz-steps">
                <li>Open <a href="https://app.fastmail.com/settings/security/tokens" target="_blank" rel="noopener">Fastmail &rarr; Settings &rarr; Privacy &amp; Security &rarr; API tokens</a>.</li>
                <li>Click <strong>New API token</strong>. Required scopes: <strong>JMAP</strong> and <strong>CalDAV</strong>.</li>
                <li>Copy the token (Fastmail only shows it once) and paste it below along with your email.</li>
            </ol>`,
    },
};

// Uniform cache shape across every provider — same keys, always present.
// The reset on wizFinish is then one assignment, no per-provider shapes.
function freshWizCache() {
    const c = { name: '', nameTouched: false };
    WIZ_ALL_FIELDS.forEach(f => { c[f] = ''; });
    return c;
}

function maskedHint(value) {
    if (!value || !value.length) return '';
    // Floor at 8 chars before exposing any tail — a short value (<8 chars) is
    // already mostly the secret if we slice 4 off the end, so just mask it
    // entirely.
    if (value.length < 8) return `<code>****</code>`;
    return `<code>****${escapeHtml(value.slice(-4))}</code>`;
}

function updateWizCachedHints() {
    const provider = WIZ_PROVIDERS[state.wizardProviderIdx];
    const cache = state.wizardCache[provider] || {};
    const setHint = (id, value) => {
        const hint = document.getElementById(id);
        if (!hint) return;
        if (value) {
            hint.innerHTML = `Saved value: ${maskedHint(value)} &middot; type to replace`;
            hint.classList.remove('hidden');
        } else {
            hint.innerHTML = '';
            hint.classList.add('hidden');
        }
    };
    setHint('wiz-client-secret-hint', cache['client-secret']);
    setHint('wiz-api-token-hint',    cache['api-token']);
}

function checkWizOverwrite() {
    const provider = WIZ_PROVIDERS[state.wizardProviderIdx];
    const nameInput = document.getElementById('wiz-name');
    const warn = document.getElementById('wiz-overwrite');
    const continueBtn = document.getElementById('wiz-continue-btn');
    const name = (nameInput?.value || '').trim();
    const existing = name ? state.accounts.find(a => a.id === name) : null;

    if (!existing || existing.id === state.wizardSavedId) {
        warn.classList.add('hidden');
        warn.classList.remove('error');
        if (continueBtn) continueBtn.disabled = false;
        return;
    }
    const label = escapeHtml(existing.email || existing.id);
    if (existing.provider !== provider) {
        // Provider mismatch — block continue. Forcing a save would clobber a
        // different-provider account; user must rename or remove the old one.
        warn.classList.add('error');
        warn.classList.remove('hidden');
        warn.innerHTML = `&#9888; The name <strong>${escapeHtml(name)}</strong> is already a <strong>${escapeHtml(existing.provider)}</strong> account (<strong>${label}</strong>). Pick a different name, or remove the existing account first.`;
        if (continueBtn) continueBtn.disabled = true;
    } else {
        warn.classList.remove('error');
        warn.classList.remove('hidden');
        warn.innerHTML = `&#9888; This will overwrite the existing <strong>${escapeHtml(existing.provider)}</strong> account <strong>${label}</strong> and replace its credentials &amp; tokens.`;
        if (continueBtn) continueBtn.disabled = false;
    }
}

function tailorWizCreds() {
    const provider = WIZ_PROVIDERS[state.wizardProviderIdx];
    const d = WIZ_DESCRIPTORS[provider];
    const cache = state.wizardCache[provider] || freshWizCache();

    // Apply provider copy (title, blurb, continueLabel, instructions).
    document.getElementById('wiz-creds-title').textContent = d.title;
    document.getElementById('wiz-creds-blurb').innerHTML = d.blurb;
    const why = document.getElementById('wiz-creds-why');
    why.innerHTML = d.instructionsHtml;
    why.style.display = '';
    document.getElementById('wiz-continue-provider').textContent = d.label;

    // Show only the fields this provider needs; reset their placeholders.
    document.querySelectorAll('.wiz-field[data-wiz-field]').forEach(f => f.classList.add('hidden'));
    document.getElementById('wiz-error').classList.add('hidden');
    d.fields.forEach(f => {
        const fieldEl = document.querySelector(`.wiz-field[data-wiz-field="${f}"]`);
        if (fieldEl) fieldEl.classList.remove('hidden');
        const inp = document.getElementById(`wiz-${f}`);
        if (inp && d.placeholders[f]) inp.placeholder = d.placeholders[f];
    });

    // Restore from cache. The name field falls back to a suggested-unique
    // default only when the user hasn't touched it (nameTouched flag —
    // explicit beats null-vs-empty-string sentinel).
    document.getElementById('wiz-name').value = cache.nameTouched ? cache.name : wizSuggestName(provider);
    WIZ_ALL_FIELDS.forEach(f => {
        const inp = document.getElementById(`wiz-${f}`);
        if (inp) inp.value = cache[f] || '';
    });

    updateWizCachedHints();
    checkWizOverwrite();

    setTimeout(() => {
        const first = document.querySelector('.wiz-screen.visible .wiz-field:not(.hidden) input');
        if (first) first.focus();
    }, 30);
}

function wizShowError(msg) {
    const el = document.getElementById('wiz-error');
    el.textContent = msg;
    el.classList.remove('hidden');
}

async function wizContinueFromCreds() {
    const provider = WIZ_PROVIDERS[state.wizardProviderIdx];
    const d = WIZ_DESCRIPTORS[provider];
    const name = document.getElementById('wiz-name').value.trim();
    if (!name) return wizShowError('Account name is required');

    // Hard re-validate cross-provider clobber even if the UI's disabled-button
    // hint was bypassed (Ctrl+Enter still fires the form submit in some
    // browsers). The user's mental model is "this will not let me clobber a
    // different-provider account" — honour it here too.
    const existing = state.accounts.find(a => a.id === name);
    if (existing && existing.provider !== provider && existing.id !== state.wizardSavedId) {
        return wizShowError(`'${name}' is already a ${existing.provider} account. Remove it first or pick a different name.`);
    }

    // Build payload from the descriptor's field list — adding a new provider
    // means adding a descriptor entry, not editing this function.
    const payload = { provider };
    for (const f of d.fields) {
        const inp = document.getElementById(`wiz-${f}`);
        const raw = inp ? inp.value : '';
        const val = (inp && inp.type === 'password') ? raw : raw.trim();
        if (!val) return wizShowError(`${WIZ_FIELD_LABELS[f] || f} is required`);
        payload[f] = val;
    }

    document.getElementById('wiz-error').classList.add('hidden');
    try {
        // Retry after Esc-back: if the account was already saved under this
        // exact name (same wizard session), skip the POST (would 409) and
        // go straight to re-authorizing. If the user renamed, delete the
        // prior id first so we don't orphan a half-set-up account.
        const sameId = state.wizardSavedId === name;
        if (state.wizardSavedId && !sameId) {
            try {
                await api('DELETE', `/accounts/${encodeURIComponent(state.wizardSavedId)}`);
            } catch (_) { /* tolerate: account may already be gone */ }
            state.wizardSavedId = null;
        }
        let resp;
        if (sameId) {
            // Re-fetch the existing record to decide if authorize is needed.
            await loadAccounts();
            const acct = state.accounts.find(a => a.id === name);
            resp = acct ? { authStatus: acct.authStatus } : { authStatus: 'pending' };
        } else {
            resp = await api('POST', `/accounts/${encodeURIComponent(name)}`, payload);
            state.wizardSavedId = name;
            state.selectedAccountId = name;
            await loadAccounts();
        }
        if (resp && resp.authStatus === 'pending') {
            wizGoTo(3);
            wizStartConnecting();
        } else {
            wizGoTo(4);
        }
    } catch (err) {
        wizShowError(err.message);
    }
}

function wizAppendLog(html) {
    const box = document.getElementById('wiz-log');
    if (!box) return;
    const line = document.createElement('div');
    line.className = 'wiz-log-line';
    line.innerHTML = html;
    box.appendChild(line);
    box.scrollTop = box.scrollHeight;
}

async function wizStartConnecting() {
    const provider = WIZ_PROVIDERS[state.wizardProviderIdx];
    const host = WIZ_DESCRIPTORS[provider].host || provider;
    document.getElementById('wiz-pulse-text').textContent = `Awaiting consent on ${host}`;
    const box = document.getElementById('wiz-log');
    box.innerHTML = '';

    // Best-effort visualisation of what the backend is doing during the
    // long-poll. These are scripted (no event stream from the server yet),
    // but they reflect the real sequence in src/platform/desktop.rs.
    const lines = [
        { d: 0,    h: `<span class="p">$</span> Generating PKCE challenge &hellip;  <span class="ok">ok</span>` },
        { d: 250,  h: `<span class="p">$</span> Binding loopback callback  <span class="ok">ok</span>` },
        { d: 500,  h: `<span class="p">$</span> Opening browser &hellip;` },
        { d: 900,  h: `<span class="p">&rarr;</span> <span class="url">https://${host}/&hellip;/auth?&hellip;</span>` },
        { d: 1400, h: `<span class="p">$</span> Awaiting redirect to <span class="in">/callback</span> &hellip; (5 min timeout)` },
    ];
    lines.forEach(e => setTimeout(() => {
        if (state.wizardStep === 3) wizAppendLog(e.h);
    }, e.d));

    if (state.authController) state.authController.abort();
    const ctrl = new AbortController();
    state.authController = ctrl;
    try {
        await api('POST', `/accounts/${encodeURIComponent(state.wizardSavedId)}/authorize`,
            null, ctrl.signal);
        wizAppendLog(`<span class="p">&larr;</span> <span class="ok">code received</span> &middot; tokens exchanged`);
        wizAppendLog(`<span class="p">$</span> Writing config &hellip;  <span class="ok">ok</span>`);
        await loadAccounts();
        setTimeout(() => { if (state.wizardStep === 3) wizGoTo(4); }, 500);
    } catch (err) {
        if (err.name === 'AbortError') return;
        wizAppendLog(`<span class="p">&times;</span> <span class="er">${escapeHtml(err.message)}</span>`);
        wizAppendLog(`<span class="p">$</span> <span class="wn">Press esc to go back and retry.</span>`);
    } finally {
        // Only clear the slot if a newer call hasn't already replaced us —
        // otherwise our late-arriving finally would clobber a fresh controller.
        if (state.authController === ctrl) state.authController = null;
    }
}

function wizCancelConnecting() {
    if (state.authController) {
        state.authController.abort();
        state.authController = null;
    }
    wizGoTo(2);
}

function renderWizSuccess() {
    const id = state.wizardSavedId;
    const acct = state.accounts.find(a => a.id === id);
    const provider = WIZ_PROVIDERS[state.wizardProviderIdx];
    const providerLabel = provider === 'gmail'   ? 'Gmail (Google)'
                        : provider === 'outlook' ? 'Outlook (Microsoft 365)'
                        :                          'Fastmail';
    document.getElementById('wiz-success-email').textContent = (acct && acct.email) || '(syncing…)';
    document.getElementById('wiz-success-provider').textContent = providerLabel;
    document.getElementById('wiz-success-name').textContent = id || '';
    document.getElementById('wiz-set-default').checked = !!(acct && acct.isDefault);
}

async function wizFinish() {
    const id = state.wizardSavedId;
    const wantDefault = document.getElementById('wiz-set-default').checked;
    const acct = state.accounts.find(a => a.id === id);
    if (wantDefault && acct && !acct.isDefault) {
        try { await setDefaultAccount(id); } catch (_) { /* swallowed; setDefault shows its own error */ }
    }
    // Clear the just-finished provider's cache so the next wizard run starts
    // fresh (otherwise "+ Add another" same provider would prefill the
    // previous account's keys). Uniform shape → one assignment.
    const provider = acct?.provider || WIZ_PROVIDERS[state.wizardProviderIdx];
    if (state.wizardCache[provider]) state.wizardCache[provider] = freshWizCache();
    closeWizard();
}

function handleWizardKey(e) {
    const step = state.wizardStep;
    const inField = !!e.target.closest && e.target.matches('input, select, textarea');

    if (step === 1) {
        if (e.key === 'Escape')                           { closeWizard(); e.preventDefault(); }
        else if (e.key === 'j' || e.key === 'ArrowDown')  { focusWizProvider(state.wizardProviderIdx + 1); e.preventDefault(); }
        else if (e.key === 'k' || e.key === 'ArrowUp')    { focusWizProvider(state.wizardProviderIdx - 1); e.preventDefault(); }
        else if (e.key === '1' || e.key === '2' || e.key === '3') {
            focusWizProvider(Number(e.key) - 1); wizGoTo(2); e.preventDefault();
        }
        else if (e.key === 'Enter')                       { wizGoTo(2); e.preventDefault(); }
        return;
    }

    if (step === 2) {
        if (e.key === 'Escape') {
            if (inField) e.target.blur();
            wizGoTo(1);
            e.preventDefault();
        } else if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
            // Ctrl/Cmd+Enter submits from anywhere in step 2 (incl. when
            // the focus is on a button). Plain Enter inside a field falls
            // through to native form submit which calls wizContinueFromCreds.
            wizContinueFromCreds();
            e.preventDefault();
        }
        return;
    }

    if (step === 3) {
        if (e.key === 'Escape') { wizCancelConnecting(); e.preventDefault(); }
        return;
    }

    if (step === 4) {
        if (inField) return;
        if (e.key === 'Enter')                           { wizFinish(); e.preventDefault(); }
        else if (e.key === 'a' || e.key === 'A')         { wizGoTo(1); e.preventDefault(); }
        else if (e.key === 'Escape')                     { closeWizard(); e.preventDefault(); }
    }
}

function showStatus(message, type = 'info') {
    els.statusMessage.textContent = message;
    els.statusMessage.style.color = type === 'error' ? 'var(--danger)' :
                                    type === 'success' ? 'var(--success)' : 'var(--fg-muted)';
    setTimeout(() => {
        els.statusMessage.textContent = '';
    }, 3000);
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
        } else if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
            saveSplit();
            e.preventDefault();
        }
        return;
    }

    // Settings: wizard owns its own key logic across steps and modes.
    if (state.view === 'settings' && state.wizardActive) {
        handleWizardKey(e);
        return;
    }

    // Settings: insert mode (editing a form field) — Ctrl+Enter saves,
    // Escape blurs the field and returns to normal mode. Other keys fall
    // through to the native input handling.
    if (state.view === 'settings' && state.mode === 'insert') {
        if (e.key === 'Escape') {
            e.target.blur();
            setMode('normal');
            e.preventDefault();
        } else if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
            saveAccount();
            e.preventDefault();
        }
        return;
    }

    // Settings: normal mode — vim-style navigation + edit triggers
    if (state.view === 'settings' && state.mode === 'normal') {
        handleSettingsNormalKey(e);
        return;
    }

    // Handle compose mode
    if (state.view === 'compose' && state.mode === 'insert') {
        if (e.key === 'Escape') {
            e.target.blur();
            setMode('normal');
            e.preventDefault();
        } else if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
            sendEmail();
            e.preventDefault();
        } else if (e.key === 'A' && e.ctrlKey && e.shiftKey) {
            els.composeFileInput.click();
            e.preventDefault();
        }
        return;
    }

    // Compose normal-mode: 'a' opens file picker instead of reply-all
    if (state.view === 'compose' && state.mode === 'normal' && e.key === 'a') {
        els.composeFileInput.click();
        e.preventDefault();
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

function handleSettingsNormalKey(e) {
    const key = e.key;
    if (key === 'Escape') {
        if (!els.acctConfirmDelete.classList.contains('hidden')) {
            els.acctConfirmDelete.classList.add('hidden');
            return;
        }
        closeSettings();
        return;
    }
    if (key === 'a') {
        beginAddAccount();
        e.preventDefault();
        return;
    }
    if (state.selectedAccountId) {
        if (key === 'd') {
            toggleConfirmDelete();
            return;
        }
        if (key === 'D') {
            setDefaultAccount(state.selectedAccountId);
            return;
        }
        if (key === 'Enter') {
            // Enter edit mode by focusing the first editable field.
            state.settingsMode = 'edit';
            renderSettings();
            // Pick the first editable visible field.
            const first = els.accountForm.querySelector(
                'input:not([readonly]):not([disabled])'
            );
            if (first) {
                first.focus();
                setMode('insert');
            }
            return;
        }
    }
    if (key === 'j' || key === 'k') {
        const dir = key === 'j' ? 1 : -1;
        const ids = state.accounts.map(a => a.id);
        if (!ids.length) return;
        const cur = ids.indexOf(state.selectedAccountId);
        const next = Math.max(0, Math.min(ids.length - 1, (cur < 0 ? 0 : cur) + dir));
        state.selectedAccountId = ids[next];
        state.settingsMode = 'edit';
        renderSettings();
        e.preventDefault();
    }
}

function handleNormalModeKey(e) {
    const key = e.key;

    // Handle g-prefix chords (gg = top, gs = settings)
    if (state.pendingG) {
        state.pendingG = false;
        if (key === 'g') {
            moveToTop();
            return;
        }
        if (key === 's') {
            openSettings();
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
            if (document.activeElement?.classList.contains('rsvp-btn')) {
                return; // Let native button click handle it
            }
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
            e.preventDefault();
            break;
        case 'a':
            startReply(true);
            e.preventDefault();
            break;
        case 'c':
            startCompose();
            e.preventDefault();
            break;
        case 'f':
            startForward();
            e.preventDefault();
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

        // RSVP shortcuts
        case 'y':
            if (state.view === 'detail' && state.currentEmail?.calendarEvent) {
                rsvpToEvent('ACCEPTED');
                e.preventDefault();
            }
            break;
        case 'n':
            if (state.view === 'detail' && state.currentEmail?.calendarEvent) {
                rsvpToEvent('DECLINED');
                e.preventDefault();
            }
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

        // Account switching (1-9) — disabled inside settings view
        case '1': case '2': case '3': case '4': case '5':
        case '6': case '7': case '8': case '9': {
            if (state.view === 'settings') break;
            const accIndex = parseInt(key) - 1;
            if (accIndex < state.accounts.length) {
                selectAccount(state.accounts[accIndex]);
                showStatus(`Switched to ${state.accounts[accIndex].email}`, 'success');
            }
            break;
        }
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
    // This handler owns all keydown events while search is open.
    // Without this, closeSearch() hides the bar mid-event and the
    // document handler sees the bar as hidden, forwarding keys to
    // normal-mode handlers (e.g. Enter -> openSelected).
    e.stopPropagation();

    const acVisible = !els.searchAutocomplete.classList.contains('hidden');
    const inputVal = els.searchInput.value;

    if (e.key === 'Enter') {
        if (acVisible) {
            acceptAutocomplete();
        } else if (inputVal.trim()) {
            // Commit token and immediately apply the search
            commitCurrentInput();
            closeSearch();
            loadEmails();
        } else if (state.searchTokens.length > 0) {
            // Empty input + tokens exist = apply search
            closeSearch();
            loadEmails();
        }
        e.preventDefault();
    } else if (e.key === 'Escape') {
        closeSearch();
        e.preventDefault();
    } else if (e.key === 'Backspace' && !inputVal) {
        if (state.searchTokens.length > 0) {
            state.searchTokens.pop();
            renderSearchChips();
        }
    } else if (e.key === 'Tab') {
        if (acVisible) {
            acceptAutocomplete();
            e.preventDefault();
        }
    } else if (e.key === 'ArrowDown') {
        if (acVisible) {
            const items = els.searchAutocomplete.querySelectorAll('.autocomplete-item');
            state.autocompleteIndex = Math.min(state.autocompleteIndex + 1, items.length - 1);
            renderAutocompleteHighlight();
            e.preventDefault();
        }
    } else if (e.key === 'ArrowUp') {
        if (acVisible) {
            state.autocompleteIndex = Math.max(0, state.autocompleteIndex - 1);
            renderAutocompleteHighlight();
            e.preventDefault();
        }
    }
}

function handleSearchInputChange() {
    const val = els.searchInput.value.toLowerCase();
    if (!val) {
        els.searchAutocomplete.classList.add('hidden');
        return;
    }

    const matches = SEARCH_OPERATORS.filter(o => o.op.startsWith(val));
    if (matches.length === 0) {
        els.searchAutocomplete.classList.add('hidden');
        return;
    }

    state.autocompleteIndex = 0;
    els.searchAutocomplete.innerHTML = matches.map((m, idx) =>
        `<div class="autocomplete-item ${idx === 0 ? 'selected' : ''}" data-index="${idx}">
            <span>${escapeHtml(m.op)}</span>
            <span class="ac-hint">${escapeHtml(m.hint)}</span>
        </div>`
    ).join('');
    els.searchAutocomplete.classList.remove('hidden');

    // Click handler for autocomplete items
    els.searchAutocomplete.querySelectorAll('.autocomplete-item').forEach(el => {
        el.addEventListener('mousedown', (e) => {
            e.preventDefault(); // prevent blur
            state.autocompleteIndex = parseInt(el.dataset.index);
            acceptAutocomplete();
        });
    });
}

// Navigation actions

function moveSelection(delta) {
    const newIndex = state.selectedIndex + delta;
    if (newIndex < 0 || newIndex >= state.emails.length) return;

    // Swap selected class directly — don't rebuild the entire list DOM.
    // j/k should be zero-cost, not O(n) innerHTML.
    const oldRow = els.emailList.querySelector(`.email-row[data-index="${state.selectedIndex}"]`);
    if (oldRow) oldRow.classList.remove('selected');

    state.selectedIndex = newIndex;

    const newRow = els.emailList.querySelector(`.email-row[data-index="${newIndex}"]`);
    if (newRow) {
        newRow.classList.add('selected');
        newRow.scrollIntoView({ block: 'nearest' });
    }

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
    // emailAction already removed the current email from state.emails,
    // so just pick the next one at the same index (or clamp to end).
    if (state.emails.length === 0) {
        showView('list');
        maybeRefillEmails();
        return;
    }

    const nextIndex = Math.min(state.selectedIndex, state.emails.length - 1);
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

    // Find the sender so we can optimistically remove all their emails
    const email = state.emails.find(e => e.id === id) || state.currentEmail;
    const senderEmail = email?.from[0]?.email?.toLowerCase();

    // Optimistic: remove all emails from this sender immediately
    let removedEmails = [];
    if (senderEmail) {
        removedEmails = state.emails.filter(e => e.from[0]?.email?.toLowerCase() === senderEmail);
        removeEmailsFromList(e => e.from[0]?.email?.toLowerCase() !== senderEmail, removedEmails.length);
    }

    showStatus('Unsubscribing and archiving...', 'info');

    // Navigate to next email immediately
    if (state.view === 'detail') {
        goToNextEmail();
    }

    try {
        const result = await api('POST', `/emails/${id}/unsubscribe-and-archive-all`);

        if (result.unsubscribeUrl) {
            window.open(result.unsubscribeUrl, '_blank');
            showStatus(`Archived ${result.archivedCount} emails from ${result.sender}. Unsubscribe page opened.`, 'success');
        } else {
            showStatus(`Archived ${result.archivedCount} emails from ${result.sender}. No unsubscribe link found.`, 'warning');
        }
        loadSplitCounts(); // resync with server truth
        maybeRefillEmails();
    } catch (err) {
        // Revert: re-insert the removed emails
        if (removedEmails.length > 0) {
            state.emails = state.emails.concat(removedEmails);
            state.emails.sort((a, b) => new Date(b.receivedAt) - new Date(a.receivedAt));
            invalidateSplitListCache();
            renderEmailList();
            adjustSplitCounts(+removedEmails.length);
        }
        showStatus('Unsubscribe failed: ' + err.message, 'error');
    }
}

function removeEmailFromList(emailId) {
    removeEmailsFromList(e => e.id !== emailId, 1);
}

function removeEmailsFromList(keepFn, expectedRemoved) {
    state.emails = state.emails.filter(keepFn);
    adjustSplitCounts(-expectedRemoved);
    invalidateSplitListCache();
    if (state.selectedIndex >= state.emails.length) {
        state.selectedIndex = Math.max(0, state.emails.length - 1);
    }
    renderEmailList();
    maybeRefillEmails();
}

// Compose

function startCompose() {
    state.replyContext = null;
    clearCompose();
    showView('compose');
}

function getComposeEmail() {
    return state.view === 'detail' ? state.currentEmail : state.emails[state.selectedIndex];
}

function startReply(replyAll) {
    const email = getComposeEmail();
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

    const quotedHtml = email.htmlBody ? sanitizeHtml(email.htmlBody) : null;
    const quotedText = email.htmlBody
        ? htmlToPlainText(email.htmlBody)
        : (email.textBody || '');

    state.replyContext = {
        inReplyTo: email.id,
        quotedHtml,
        quotedText,
    };

    autoSelectFromAddress(email);

    const header = `On ${formatDate(email.receivedAt)}, ${from?.name || from?.email} wrote:`;
    if (quotedHtml) {
        els.composeQuote.innerHTML = `<p><strong>${escapeHtml(header)}</strong></p>${quotedHtml}`;
    } else {
        els.composeQuote.innerHTML = `<p><strong>${escapeHtml(header)}</strong></p><pre>${escapeHtml(quotedText)}</pre>`;
    }
    els.composeQuote.classList.remove('hidden');

    showView('compose');
}

function startForward() {
    const email = getComposeEmail();
    if (!email) return;

    clearCompose();
    autoSelectFromAddress(email);

    els.composeSubject.value = email.subject.startsWith('Fwd:') ? email.subject : `Fwd: ${email.subject}`;

    const from = email.from[0];
    const quotedHtml = email.htmlBody ? sanitizeHtml(email.htmlBody) : null;
    const quotedText = email.htmlBody
        ? htmlToPlainText(email.htmlBody)
        : (email.textBody || '');

    state.replyContext = { quotedHtml, quotedText };

    const headerLines = `---------- Forwarded message ---------<br>From: ${escapeHtml(from?.name || '')} &lt;${escapeHtml(from?.email)}&gt;<br>Subject: ${escapeHtml(email.subject)}`;
    if (quotedHtml) {
        els.composeQuote.innerHTML = `<p>${headerLines}</p>${quotedHtml}`;
    } else {
        els.composeQuote.innerHTML = `<p>${headerLines}</p><pre>${escapeHtml(quotedText)}</pre>`;
    }
    els.composeQuote.classList.remove('hidden');

    showView('compose');
}

function clearCompose() {
    els.composeTo.value = '';
    els.composeCc.value = '';
    els.composeSubject.value = '';
    els.composeBody.value = '';
    els.composeQuote.innerHTML = '';
    els.composeQuote.classList.add('hidden');
    if (els.composeFrom && state.identities.length) {
        els.composeFrom.value = state.identities[0].email;
    }
    state.replyContext = null;
    // Clear pending attachments and abort any in-progress uploads
    for (const att of state.pendingAttachments) {
        if (att.controller) att.controller.abort();
    }
    state.pendingAttachments = [];
    els.composeAttachments.classList.add('hidden');
    els.composeAttachmentsList.innerHTML = '';
    els.composeFileInput.value = '';
    // Reset invite-compose fields
    if (els.composeInviteEnabled) {
        els.composeInviteEnabled.checked = false;
        els.composeInviteFields.classList.add('hidden');
        els.inviteSummary.value = '';
        els.inviteLocation.value = '';
        els.inviteStart.value = '';
        els.inviteEnd.value = '';
        els.inviteTz.value = '';
    }
}

let attachmentIdCounter = 0;

function handleFileSelect() {
    const files = els.composeFileInput.files;
    if (!files.length) return;
    addFiles(files);
    els.composeFileInput.value = '';
}

function uploadAttachment(file, id, controller) {
    const xhr = new XMLHttpRequest();
    xhr.open('POST', '/api/upload');
    xhr.setRequestHeader('Content-Type', file.type || 'application/octet-stream');
    xhr.setRequestHeader('X-Filename', file.name);

    xhr.upload.onprogress = (e) => {
        if (!e.lengthComputable) return;
        const att = state.pendingAttachments.find(a => a._id === id);
        if (att) {
            att.progress = Math.round((e.loaded / e.total) * 100);
            renderComposeAttachments();
        }
    };

    xhr.onload = () => {
        if (xhr.status < 200 || xhr.status >= 300) {
            const att = state.pendingAttachments.find(a => a._id === id);
            if (att) {
                att.status = 'error';
                att.controller = null;
                renderComposeAttachments();
                showStatus(`Upload failed: ${file.name}`, 'error');
            }
            return;
        }
        let data;
        try { data = JSON.parse(xhr.responseText); } catch {
            const att = state.pendingAttachments.find(a => a._id === id);
            if (att) { att.status = 'error'; att.controller = null; renderComposeAttachments(); showStatus(`Upload failed: ${file.name}`, 'error'); }
            return;
        }
        const att = state.pendingAttachments.find(a => a._id === id);
        if (att) {
            att.blob_id = data.blob_id;
            att.status = 'ready';
            att.progress = 100;
            att.controller = null;
            renderComposeAttachments();
        }
    };

    xhr.onerror = () => {
        const att = state.pendingAttachments.find(a => a._id === id);
        if (att) {
            att.status = 'error';
            att.controller = null;
            renderComposeAttachments();
            showStatus(`Upload failed: ${file.name}`, 'error');
        }
    };

    // Wire abort through the controller
    controller.signal.addEventListener('abort', () => xhr.abort());

    xhr.send(file);
}

function renderComposeAttachments() {
    if (!state.pendingAttachments.length) {
        els.composeAttachments.classList.add('hidden');
        els.composeAttachmentsList.innerHTML = '';
        return;
    }
    els.composeAttachments.classList.remove('hidden');
    els.composeAttachmentsList.innerHTML = state.pendingAttachments.map(att => {
        const icon = getFileIcon(att.mime_type, att.name);
        const size = formatFileSize(att.size);
        const statusIcon = att.status === 'uploading' ? '\u23F3'
            : att.status === 'error' ? '\u274C' : '\u2705';
        const progressBar = att.status === 'uploading'
            ? `<div class="attachment-progress"><div class="attachment-progress-bar" style="width: ${att.progress || 0}%"></div></div>`
            : '';
        return `<div class="compose-attachment-item" data-id="${att._id}">
            <span class="attachment-icon">${icon}</span>
            <span class="attachment-name">${escapeHtml(att.name)}</span>
            <span class="attachment-size">${size}</span>
            <span class="attachment-status">${statusIcon}</span>
            <span class="attachment-remove" data-id="${att._id}">\u00D7</span>
            ${progressBar}
        </div>`;
    }).join('');
}

function handleAttachmentListClick(e) {
    const removeBtn = e.target.closest('.attachment-remove');
    if (!removeBtn) return;
    const id = parseInt(removeBtn.dataset.id);
    const idx = state.pendingAttachments.findIndex(a => a._id === id);
    if (idx === -1) return;
    const att = state.pendingAttachments[idx];
    if (att.controller) att.controller.abort();
    state.pendingAttachments.splice(idx, 1);
    renderComposeAttachments();
}

function setupComposeDragDrop() {
    els.composeView.addEventListener('dragenter', (e) => {
        if (state.view !== 'compose') return;
        e.preventDefault();
        els.composeView.classList.add('drag-over');
    });
    els.composeView.addEventListener('dragover', (e) => {
        if (state.view !== 'compose') return;
        e.preventDefault();
        els.composeView.classList.add('drag-over');
    });
    els.composeView.addEventListener('dragleave', (e) => {
        if (e.target !== els.composeView && els.composeView.contains(e.relatedTarget)) return;
        els.composeView.classList.remove('drag-over');
    });
    els.composeView.addEventListener('drop', (e) => {
        e.preventDefault();
        els.composeView.classList.remove('drag-over');
        if (state.view !== 'compose') return;
        const files = e.dataTransfer.files;
        if (!files.length) return;
        addFiles(files);
    });
}

function handleComposePaste(e) {
    const files = e.clipboardData?.files;
    if (!files || !files.length) return;
    e.preventDefault();
    const toAdd = [];
    for (const file of files) {
        const name = file.name && file.name !== 'image.png'
            ? file.name
            : `pasted-image-${Date.now()}.png`;
        toAdd.push(new File([file], name, { type: file.type }));
    }
    addFiles(toAdd);
}

function addFiles(files) {
    for (const file of files) {
        if (file.size > 25 * 1024 * 1024) {
            showStatus(`${file.name} is too large (max 25 MB)`, 'error');
            continue;
        }
        const id = ++attachmentIdCounter;
        const controller = new AbortController();
        state.pendingAttachments.push({
            _id: id,
            name: file.name,
            mime_type: file.type || 'application/octet-stream',
            size: file.size,
            status: 'uploading',
            progress: 0,
            controller,
        });
        renderComposeAttachments();
        uploadAttachment(file, id, controller);
    }
}

function autoSelectFromAddress(email) {
    if (!els.composeFrom || !state.identities.length) return;
    // Check To first, then CC — To matches always take priority over CC matches
    const lists = [email.to || [], email.cc || []];
    for (const list of lists) {
        for (const r of list) {
            if (!r.email) continue;
            const addr = r.email.toLowerCase();
            for (const id of state.identities) {
                if (id.email.toLowerCase() === addr) {
                    els.composeFrom.value = id.email;
                    return;
                }
            }
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
        { name: 'Add Account', desc: 'Connect a new mailbox', shortcut: '', action: 'add-account' },
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

    // Add remove commands for each existing account
    state.accounts.forEach(acct => {
        const label = acct.email || acct.id;
        commands.push({
            name: `Remove Account: ${label}`,
            desc: `Disconnect and delete cached tokens for ${label}`,
            shortcut: '',
            action: `remove-account:${acct.id}`,
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
        case 'add-account':
            openSettings();
            openWizard();
            break;
        default:
            // Handle dynamic delete-split commands
            if (action.startsWith('delete-split:')) {
                const splitId = action.replace('delete-split:', '');
                deleteSplit(splitId);
            } else if (action.startsWith('remove-account:')) {
                const id = action.slice('remove-account:'.length);
                removeAccountById(id);
            }
            break;
    }
}

async function removeAccountById(id) {
    const acct = state.accounts.find(a => a.id === id);
    const label = (acct && acct.email) || id;
    if (!window.confirm(`Remove account "${label}"? This deletes cached tokens.`)) return;
    try {
        await api('DELETE', `/accounts/${encodeURIComponent(id)}`);
        showStatus(`Deleted ${id}`, 'success');
        if (state.selectedAccountId === id) {
            state.selectedAccountId = null;
            state.settingsMode = 'view';
        }
        if (state.currentAccount === id) {
            state.currentAccount = null;
            state.currentEmail = null;
            state.emails = [];
        }
        await loadAccounts();
    } catch (err) {
        showStatus(`Failed to delete ${id}: ${err.message}`, 'error');
    }
}

// Search

function openSearch() {
    els.searchBar.classList.remove('hidden');
    els.searchInput.value = '';
    renderSearchChips();
    els.searchAutocomplete.classList.add('hidden');
    els.searchInput.focus();
    setMode('search');
}

function closeSearch() {
    els.searchBar.classList.add('hidden');
    els.searchAutocomplete.classList.add('hidden');
    updateActiveFilters();
    setMode('normal');
}

function getSearchQuery() {
    return state.searchTokens.map(t => {
        const sanitized = t.value.replace(/"/g, '');
        if (!sanitized) return '';
        if (t.type === 'text') {
            return sanitized.includes(' ') ? `"${sanitized}"` : sanitized;
        }
        const val = sanitized.includes(' ') ? `"${sanitized}"` : sanitized;
        return `${t.type}:${val}`;
    }).filter(Boolean).join(' ');
}

function commitCurrentInput() {
    const raw = els.searchInput.value.trim();
    if (!raw) return;

    // Check if input matches operator:value pattern
    const colonIdx = raw.indexOf(':');
    if (colonIdx > 0) {
        const prefix = raw.substring(0, colonIdx).toLowerCase();
        const value = raw.substring(colonIdx + 1);
        const rawLower = raw.toLowerCase();
        // Check if it's a known operator
        const knownOp = SEARCH_OPERATORS.find(o => o.op === prefix + ':' || o.op === rawLower);
        if (knownOp) {
            if (!knownOp.needsValue) {
                // Complete token like has:attachment
                const parts = knownOp.op.split(':');
                state.searchTokens.push({ type: parts[0], value: parts.slice(1).join(':') });
            } else if (value) {
                state.searchTokens.push({ type: knownOp.op.split(':')[0], value });
            } else {
                // Operator typed but no value yet — leave in input
                return;
            }
            els.searchInput.value = '';
            renderSearchChips();
            return;
        }
    }

    // Plain text token (including unknown operator-like input)
    state.searchTokens.push({ type: 'text', value: raw });
    els.searchInput.value = '';
    renderSearchChips();
}

function acceptAutocomplete() {
    const items = els.searchAutocomplete.querySelectorAll('.autocomplete-item');
    if (items.length === 0) return;

    const idx = Math.min(state.autocompleteIndex, items.length - 1);
    const opText = items[idx].querySelector('span').textContent;
    const op = SEARCH_OPERATORS.find(o => o.op === opText);

    if (op && !op.needsValue) {
        // Complete token — e.g. has:attachment, is:unread
        const parts = op.op.split(':');
        state.searchTokens.push({ type: parts[0], value: parts.slice(1).join(':') });
        els.searchInput.value = '';
        renderSearchChips();
    } else {
        // Needs value — put operator in input for user to type value
        els.searchInput.value = opText;
        // Move cursor to end
        els.searchInput.setSelectionRange(opText.length, opText.length);
    }
    els.searchAutocomplete.classList.add('hidden');
}

function renderAutocompleteHighlight() {
    const items = els.searchAutocomplete.querySelectorAll('.autocomplete-item');
    items.forEach((el, idx) => {
        el.classList.toggle('selected', idx === state.autocompleteIndex);
    });
}

function renderChips(tokens, container, opts = {}) {
    container.innerHTML = tokens.map((t, idx) => {
        const label = t.type === 'text' ? t.value : `${t.type}:${t.value}`;
        const removeBtn = opts.removable !== false
            ? `<span class="chip-remove" data-index="${idx}">&times;</span>`
            : '';
        return `<span class="search-chip">${escapeHtml(label)}${removeBtn}</span>`;
    }).join('');
}

function renderSearchChips() {
    renderChips(state.searchTokens, els.searchTokens);
}

function updateActiveFilters() {
    if (state.searchTokens.length > 0) {
        renderChips(state.searchTokens, els.activeFilterChips);
        els.activeFilters.classList.remove('hidden');
    } else {
        els.activeFilters.classList.add('hidden');
    }
}

function clearAllFilters() {
    state.searchTokens = [];
    updateActiveFilters();
    loadEmails();
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

function pushUndo(action, emailId, emailData, insertIndex) {
    state.undoStack.push({ action, emailId, emailData, insertIndex, timestamp: Date.now() });

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

    els.undoToast.classList.add('hidden');
    showStatus('Undone', 'success');

    // Optimistic: re-insert the email into the list immediately
    if (item.emailData) {
        const idx = Math.min(item.insertIndex, state.emails.length);
        state.emails.splice(idx, 0, item.emailData);
        state.selectedIndex = idx;
        invalidateSplitListCache();
        renderEmailList();

    }
    adjustSplitCounts(+1);

    try {
        const inbox = state.mailboxes.find(m => m.role === 'inbox');
        if (inbox) {
            await api('POST', `/emails/${item.emailId}/move`, { mailbox_id: inbox.id });
        }
        loadSplitCounts(); // resync with server truth
    } catch (err) {
        // Revert: remove the email we optimistically re-inserted
        if (item.emailData) {
            removeEmailFromList(item.emailId);
        }
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

// escapeHtml is safe for text content but textContent's serializer doesn't
// encode `"` or `'`, so a value with quotes can break out of an attribute.
// Use escapeAttr inside attribute strings like data-foo="${...}".
function escapeAttr(text) {
    return escapeHtml(text).replace(/"/g, '&quot;').replace(/'/g, '&#39;');
}

// Strip color-related CSS properties from inline styles.
// Preserves layout (margin, padding, display) while removing colors.
function stripColorStyles(styleString) {
    const colorProps = [
        'color', 'background-color',
        'outline-color', 'text-decoration-color', 'text-shadow', 'box-shadow'
    ];
    return styleString.split(';')
        .map(d => d.trim())
        .flatMap(d => {
            if (!d) return [];
            const propName = d.split(':')[0]?.trim().toLowerCase();
            if (!propName) return [];
            // Always preserve background-image (contains url())
            if (propName === 'background-image') return [d];
            // For background shorthand: extract url() as background-image, strip color parts
            if (propName === 'background') {
                const urlMatch = d.match(/url\s*\([^)]*\)/i);
                return urlMatch ? ['background-image: ' + urlMatch[0]] : [];
            }
            if (colorProps.some(cp => propName === cp || propName.startsWith(cp + '-'))) return [];
            return [d];
        })
        .join('; ');
}

function sanitizeStyleContent(css) {
    // Remove @import rules (external resource loading / tracking)
    css = css.replace(/@import\b[^;]*;?/gi, '');
    // Remove @font-face rules (external resource loading)
    css = css.replace(/@font-face\s*\{[^}]*\}/gi, '');
    // Strip external url() references (tracking pixels) but preserve cid: URLs (inline images)
    css = css.replace(/url\s*\(\s*(?!['"]?cid:)[^)]*\)/gi, '');
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

    // Linkify bare URLs in text nodes
    const walker = doc.createTreeWalker(doc.body, NodeFilter.SHOW_TEXT);
    const textNodes = [];
    while (walker.nextNode()) textNodes.push(walker.currentNode);
    for (const node of textNodes) {
        if (node.parentElement && node.parentElement.closest('a')) continue;
        const segments = segmentUrls(node.textContent, true);
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
    doc.querySelectorAll('a[href]').forEach(el => {
        el.setAttribute('target', '_blank');
        el.setAttribute('rel', 'noopener noreferrer');
    });

    return doc.body.innerHTML;
}

// Strips HTML tags and returns plain text. Uses innerText to preserve
// block-level boundaries (p, br, div) as newlines. Output is safe for
// text contexts only (textarea.value) — do not insert via innerHTML.
function htmlToPlainText(html) {
    const doc = new DOMParser().parseFromString(html, 'text/html');
    return doc.body.innerText || '';
}

function segmentUrls(text, raw) {
    const re = raw ? /https?:\/\/[^\s<>"')\]]+/g : /https?:\/\/[^\s<>&"')\]]+/g;
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
    return segmentUrls(text, true).map(p => p.url
        ? `<a href="${escapeHtml(p.url)}" target="_blank" rel="noopener noreferrer">${escapeHtml(p.url)}</a>`
        : escapeHtml(p.text)
    ).join('');
}

// Attachment functions

function renderAttachments(attachments, emailId) {
    els.attachments.classList.remove('hidden');
    const downloadAllBtn = attachments.length > 1
        ? `<a class="attachments-download-all" onclick="downloadAllAttachments(event)">Download All</a>`
        : '';
    const header = `<div class="attachments-header"><span>📎 Attachments (${attachments.length})</span>${downloadAllBtn}</div>`;
    const items = attachments.map(att => {
        const icon = getFileIcon(att.mime_type, att.name);
        const size = formatFileSize(att.size);
        const url = `/api/emails/${emailId}/attachments/${encodeURIComponent(att.blob_id)}/${encodeURIComponent(att.name)}`;
        return `
            <a class="attachment-item" href="${url}" download="${escapeHtml(att.name)}">
                <span class="attachment-icon">${icon}</span>
                <span class="attachment-name">${escapeHtml(att.name)}</span>
                <span class="attachment-size">${size}</span>
                <span class="attachment-download">&#8615;</span>
            </a>
        `;
    }).join('');
    els.attachmentsList.innerHTML = header + items;
}

function downloadAllAttachments(e) {
    e.preventDefault();
    const links = els.attachmentsList.querySelectorAll('.attachment-item');
    links.forEach((a, i) => {
        setTimeout(() => a.click(), i * 200);
    });
}

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

// Calendar functions

function renderCalendarCard(event) {
    els.calendarEvent.classList.remove('hidden');
    const cancelled = event.method === 'CANCEL';
    const card = els.calendarEvent.querySelector('.calendar-card');
    card.classList.toggle('cancelled', cancelled);

    els.calTitle.textContent = event.summary || 'Calendar Event';
    els.calDatetime.innerHTML = formatEventTimeMultiTz(event.dtstart, event.dtend);
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

    // Find current user's RSVP status
    const userStatus = event.user_rsvp_status || getUserRsvpStatus(event);

    // Hide RSVP actions for cancelled events
    const actions = els.calendarEvent.querySelector('.calendar-actions');
    if (cancelled) {
        actions.style.display = 'none';
    } else {
        actions.style.display = '';
        // Highlight active button
        els.rsvpAccept.classList.toggle('active', userStatus === 'ACCEPTED');
        els.rsvpMaybe.classList.toggle('active', userStatus === 'TENTATIVE');
        els.rsvpDecline.classList.toggle('active', userStatus === 'DECLINED');
    }

    // Show "You responded" label
    const statusLabel = document.getElementById('rsvp-status-label');
    if (statusLabel) {
        if (userStatus && userStatus !== 'NEEDS-ACTION') {
            const label = { ACCEPTED: 'Accepted', TENTATIVE: 'Maybe', DECLINED: 'Declined' }[userStatus];
            statusLabel.textContent = `You responded ${label}`;
            statusLabel.classList.remove('hidden');
        } else {
            statusLabel.classList.add('hidden');
        }
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

function formatEventTime(dtstart, dtend, timeZone) {
    if (!dtstart) return '';
    const start = new Date(dtstart);
    const options = {
        weekday: 'short',
        month: 'short',
        day: 'numeric',
        hour: 'numeric',
        minute: '2-digit',
        timeZoneName: 'short'
    };
    if (timeZone) options.timeZone = timeZone;
    let result = start.toLocaleString(undefined, options);

    if (dtend) {
        const end = new Date(dtend);
        const endTimeOpts = { hour: 'numeric', minute: '2-digit' };
        if (timeZone) endTimeOpts.timeZone = timeZone;
        const sameDay = sameDayInTz(start, end, timeZone);
        if (sameDay) {
            result += ' – ' + end.toLocaleTimeString(undefined, endTimeOpts);
        } else {
            result += ' – ' + end.toLocaleString(undefined, options);
        }
    }
    return result;
}

function sameDayInTz(a, b, timeZone) {
    if (!timeZone) return a.toDateString() === b.toDateString();
    const fmt = new Intl.DateTimeFormat(undefined, {
        timeZone, year: 'numeric', month: '2-digit', day: '2-digit'
    });
    return fmt.format(a) === fmt.format(b);
}

function formatEventTimeMultiTz(dtstart, dtend) {
    const zones = (state.timezone && state.timezone.display && state.timezone.display.length)
        ? state.timezone.display
        : [undefined];  // fall back to browser local
    return zones.map((tz, i) => {
        const line = formatEventTime(dtstart, dtend, tz);
        const cls = i === 0 ? 'event-time primary' : 'event-time secondary';
        return `<div class="${cls}">${escapeHtml(line)}</div>`;
    }).join('');
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

    const event = state.currentEmail.calendarEvent;
    if (event?.user_rsvp_status === status) return; // already at this status — no-op

    const label = { ACCEPTED: 'Accepted', TENTATIVE: 'Maybe', DECLINED: 'Declined' }[status] || status;
    let prevEvent = null;

    // Optimistic: update RSVP buttons immediately if we have event data
    if (event) {
        prevEvent = JSON.parse(JSON.stringify(event));
        event.user_rsvp_status = status;
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

// Initialize on load
document.addEventListener('DOMContentLoaded', init);

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
    // Contact autocomplete on compose To/Cc (kata e64s, task B6) — client-side
    // only, no server surface. Harvested from loaded email list pages plus one
    // background Sent-mailbox fetch per account session (see harvestContacts /
    // harvestSentContactsOnce). Account-scoped like every other cache in this
    // file (see selectAccount): accountId -> Map of
    // email(lowercased) -> {email, name, lastSeen, count}. Access via
    // contactIndexFor(accountId) — never share entries across accounts.
    contactIndex: new Map(),
    contactAcField: null,     // 'to' | 'cc' | null — which compose field's dropdown is open
    contactAcIndex: 0,        // highlighted row in the open contact dropdown
    undoStack: [],
    // Threading / conversation grouping in the desktop list view (kata 64z6,
    // task B7) — client-side v1, no server Thread/get. threadGroups is built
    // incrementally at APPEND time (see extendThreadGroups / rebuildThreadGroups)
    // so grouping never costs an O(n^2) per-render scan: Map threadId -> ordered
    // array of the email ids seen for that thread. It is append-only within a
    // loaded list (never pruned on archive) — visibleRows() re-derives the LIVE
    // present-member set from state.emails each render, so a member being
    // archived/undone stays correct without touching this map. expandedThreads
    // holds the threadIds the user has expanded inline; it resets on every full
    // list replace (see rebuildThreadGroups).
    threadGroups: new Map(),
    expandedThreads: new Set(),
    pendingG: false,          // for gg command
    commandPaletteIndex: 0,
    replyContext: null,       // for reply/forward
    draftId: null,            // server id of the persistent draft this compose is autosaving (kata wm57)
    composeBaseline: '',      // compose body value at clear/restore time; composeDirty compares
                              // against it so an untouched signature prefill (or restored draft)
                              // never reads as a change worth autosaving
    composeSession: 0,        // bumped by clearCompose on every fresh/restored compose; an
                              // in-flight autosave adopts its returned id only while the token
                              // still matches, so a stale save can't corrupt a newer draft
    sending: false,           // true while sendEmail's request is in flight; runAutosave bails
                              // on it so a debounce firing mid-send can't persist a ghost
                              // draft of the very mail being sent
    identities: [],           // send-as email addresses
    splits: [],               // split inbox definitions
    currentSplit: 'all',      // currently active split tab
    pendingAttachments: [],   // files being uploaded for compose
    splitCounts: {},          // email counts per split tab
    starredOnly: false,       // sidebar "Starred" filter — restricts list to $flagged emails
    sortOrder: 'date_desc',   // list sort: 'date_desc' (newest first, default) | 'date_asc' (oldest first)
                              // session-only — resets to default on account switch (see selectAccount)
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

// Simple cache: email id -> full email object with body
const emailCache = {};
// Scroll position cache: email id -> scrollTop
const scrollPositions = {};

// Opening the palette blurs insert-mode fields. Keep the pre-palette focus and
// mode outside the command state so Escape can restore the exact editing
// context without an executed action being able to reuse stale focus.
let commandPalettePreviousFocus = null;
let commandPalettePreviousMode = 'normal';

// Rolling email cache
const CACHE_LIMIT = 150;

// Deadline guard consumed by the window focus listener: set when the
// email-iframe focus-bounce returns focus to this window, so that synthetic
// focus event doesn't re-fetch the theme on every click into an email body.
// Time-bounded rather than one-shot: if the OS window loses focus before
// the bounce fires, no synthetic focus ever arrives, and a stale boolean
// would swallow the next REAL alt-tab-back refresh (roborev 313).
let suppressFocusThemeRefreshUntil = 0;
const REFILL_THRESHOLD = 100;
let refillInFlight = false;

// Per-split email list cache for instant split switching
// Key: "accountId:mailboxId:splitId:search" -> email array
const splitListCache = {};
let loadEmailsController = null;

// Contact autocomplete harvesting (kata e64s, task B6). All module-level and
// session-only (reset on page reload), mirroring emailCache/splitListCache.
// `${accountId}:${emailId}` keys already folded into state.contactIndex — a
// re-render of an already-fetched page (e.g. the splitListCache instant-switch
// snapshot) must not double-count the same message.
const harvestedMessageIds = new Set();
// accountIds whose Sent-mailbox background fetch has already been attempted
// this session (attempted, not necessarily successful — see
// harvestSentContactsOnce: a failure isn't retried).
const sentHarvestedAccounts = new Set();
// Lowercased addresses excluded from contact suggestions — accumulates across
// every account whose identities have been loaded this session. Deliberately
// NOT account-scoped, unlike contactIndex: it is exclusion-only, so
// over-excluding another account's "you" fails safe.
const ownIdentityEmails = new Set();
// Rows currently rendered in whichever contact dropdown is open.
let contactAcMatches = [];

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
    els.composeToAutocomplete = document.getElementById('compose-to-autocomplete');
    els.composeCcAutocomplete = document.getElementById('compose-cc-autocomplete');
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
    els.deployBanner = document.getElementById('deploy-banner');
    els.deployRefreshBtn = document.getElementById('deploy-refresh-btn');
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
    els.sortToggle = document.getElementById('sort-toggle');
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
    els.acctAppPassword = document.getElementById('acct-app-password');
    els.acctClientId = document.getElementById('acct-client-id');
    els.acctClientSecret = document.getElementById('acct-client-secret');
    els.acctSignature = document.getElementById('acct-signature');
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
    if (els.sortToggle) {
        els.sortToggle.addEventListener('click', toggleSortOrder);
    }
    renderSortToggle();
    els.accountErrorBanner.querySelector('.error-banner-dismiss').addEventListener('click', () => {
        els.accountErrorBanner.classList.add('hidden');
    });
    document.addEventListener('keydown', handleKeyDown);
    // The sandboxed email-body iframe (renderHtmlBodyIframe) swallows
    // keyboard focus when clicked: it's cross-origin by design (no
    // allow-same-origin), so once it holds focus every shortcut — Escape
    // back to the list, j/k, all of it — silently dies, with no way back
    // but the mouse. Focus moving into any iframe fires window blur;
    // bounce it straight back. Mouse text-selection inside the email
    // doesn't need keyboard focus, so reading and selecting still work
    // (copying that selection needs the right-click menu).
    window.addEventListener('blur', () => {
        setTimeout(() => {
            const el = document.activeElement;
            if (el?.classList?.contains('email-iframe')) {
                // Bouncing focus back fires a window focus event; suppress
                // the theme refresh for the synthetic wake only. The short
                // deadline (not a one-shot flag) means a real alt-tab-back
                // is never swallowed if the synthetic event doesn't arrive.
                suppressFocusThemeRefreshUntil = Date.now() + 50;
                el.blur();
            }
        }, 0);
    });
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
        // Threading (kata 64z6): a click on a collapsed thread's count badge
        // toggles inline expansion instead of opening the newest message.
        const countBadge = e.target.closest('.email-thread-count');
        if (countBadge) {
            toggleThreadExpand(countBadge.dataset.thread);
            return;
        }
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
        // Debounced draft autosave (kata wm57): each edit reschedules the save.
        el.addEventListener('input', scheduleAutosave);
    });

    // Contact autocomplete on To/Cc (kata e64s, task B6) — separate listeners
    // from the generic block above so the dropdown wiring stays out of the
    // unrelated Subject/Body autosave path.
    els.composeTo.addEventListener('input', () => handleContactFieldInput('to'));
    els.composeCc.addEventListener('input', () => handleContactFieldInput('cc'));
    els.composeTo.addEventListener('blur', closeContactAutocomplete);
    els.composeCc.addEventListener('blur', closeContactAutocomplete);

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
        'wiz-app-password':  'app-password',
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
            if (id === 'wiz-client-secret' || id === 'wiz-api-token' || id === 'wiz-app-password')
                updateWizCachedHints();
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
    // Reload theme on window focus (pick up theme changes after alt-tabbing
    // back). The iframe focus-bounce below hands focus back to this window,
    // which fires a synthetic focus event — without the suppress flag every
    // click into an email body would re-fetch and re-apply the theme
    // (roborev 312 #2).
    window.addEventListener('focus', () => {
        if (Date.now() < suppressFocusThemeRefreshUntil) {
            suppressFocusThemeRefreshUntil = 0;
            return;
        }
        loadTheme();
        // Re-check for a deploy on focus: a common flow is alt-tabbing away,
        // upgrading, then alt-tabbing back — a poll here surfaces the banner
        // immediately instead of waiting up to DEPLOY_POLL_INTERVAL_MS.
        checkDeploy();
    });

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
    loadTimezone();
    loadTzZones();
    // Start the deploy-detection poll so a banner appears when a new version
    // is deployed (Linear / Monarch Money style). Captures the boot build id
    // first, then re-checks on an interval and on window focus.
    startDeployPoll();
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

// API calls — the client itself lives in the shared api.js (loaded before
// this script; makeApi/ApiError/ApiAuthError are its globals). The account
// is read at call time because desktop switches accounts in-place.

function api(method, path, body = null, signal = null) {
    return makeApi(state.currentAccount?.id)(method, path, body, signal);
}

// Like api(), but resolves to { data, headers } — for the one caller that
// needs a response header (loadEmails' stale-snapshot detection).
function apiWithMeta(method, path, body = null, signal = null) {
    return makeApi(state.currentAccount?.id).withMeta(method, path, body, signal);
}

// Dismiss the cold-boot splash (kata qknk). #boot-splash is shipped in the
// initial HTML so it paints on the first frame; this fades it out (~150ms via
// the CSS transition on .hide) and removes it from the DOM once the first
// meaningful boot state is ready (loadAccounts resolving). One-shot: a no-op
// once the splash is gone, so it's safe to call from every loadAccounts path
// (refresh, account add, etc.).
function hideBootSplash() {
    const splash = document.getElementById('boot-splash');
    if (!splash) return;
    splash.classList.add('hide');
    const drop = () => splash.remove();
    splash.addEventListener('transitionend', drop, { once: true });
    // Fallback if transitionend never fires (e.g. the element is detached
    // mid-transition) — remove after the fade window. remove() is a no-op on
    // an already-detached element, so the two paths can't conflict.
    setTimeout(drop, 200);
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
            hideBootSplash();
            return;
        }

        // Auto-select only a connected account — selecting a pending one
        // would fire mailbox fetches that can only fail.
        const connected = state.accounts.filter(a => a.authStatus !== 'pending');
        const defaultAcc = connected.find(a => a.isDefault) || connected[0];
        if (defaultAcc) {
            selectAccount(defaultAcc);
        } else {
            // Accounts exist but none are authorized — land in settings so
            // the user can complete authorization.
            state.currentAccount = null;
            state.currentMailbox = null;
            state.emails = [];
            els.mailboxName.textContent = 'NOT AUTHORIZED';
            openSettings();
        }

        // If we were already in settings (e.g. just completed first-run save),
        // re-render to show the new account list rather than the firstRun pane.
        if (state.view === 'settings') renderSettings();
        // Boot resolved — dismiss the splash so the (now populated) shell is
        // visible. selectAccount/openSettings above have run, so the shell's
        // own loading states (e.g. #email-list .loading) take over from here.
        hideBootSplash();
    } catch (err) {
        showStatus('Failed to load accounts: ' + err.message, 'error');
        // A boot error dismisses the splash so the error banner/status is
        // visible instead of hanging on the splash forever.
        hideBootSplash();
    }
}

function showAccountErrors(errors) {
    const count = errors.length;
    const list = errors.map(e => {
        const acctText = escapeHtml(e.account);
        const acctAttr = escapeAttr(e.account);
        const prov = e.provider ? providerIcon(e.provider) : '';
        const body = escapeHtml(e.error);
        // The Authorize button is purely structural — gated on authStatus,
        // independent of error text. The backend can reword "Not authorized
        // — click Authorize" however it wants and the button still appears.
        const acctRec = state.accounts.find(a => a.id === e.account);
        const needsAuth = acctRec && acctRec.authStatus === 'pending';
        // Fastmail has no OAuth flow — its button opens the edit form, so
        // label it accordingly (authorizeAccountFromBanner branches the same
        // way and never fires the doomed /authorize request).
        const label = acctRec?.provider === 'fastmail' ? '[ Fix ]' : '[ Authorize ]';
        const action = needsAuth
            ? ` <button type="button" class="banner-authorize-link" data-account-id="${acctAttr}">${label}</button>`
            : '';
        return `<li><strong>${acctText}</strong>${prov ? ` (${prov})` : ''}: ${body}${action}</li>`;
    }).join('');
    // "failed to connect" is wrong for non-connection notices like the
    // stale-config banner (provider "config") — use a neutral heading then.
    const allConnect = errors.every(e => e.provider && e.provider !== 'config');
    const heading = allConnect
        ? `${count} account${count > 1 ? 's' : ''} failed to connect:`
        : `${count} item${count > 1 ? 's need' : ' needs'} attention:`;
    els.accountErrorDetails.innerHTML =
        `<strong>${heading}</strong><ul>${list}</ul>`;
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
    if (acct.provider === 'fastmail') {
        // Fastmail doesn't use OAuth — a session-less Fastmail account means
        // the connection failed (bad token, network). Land on the edit form;
        // POSTing /authorize would only 400.
        showStatus(`${id} failed to connect — check the username and API token`, 'error');
        return;
    }
    showStatus(`Authorizing ${id}…`, 'info');
    authorize(id);
}

function renderAccounts() {
    if (state.accounts.length <= 1) {
        els.accountSelector.style.display = 'none';
        return;
    }

    els.accountSelector.style.display = 'block';
    els.accountSelector.innerHTML = state.accounts.map((acc, idx) => {
        const pending = acc.authStatus === 'pending';
        return `
        <div class="account-item ${state.currentAccount?.id === acc.id ? 'active' : ''}${pending ? ' pending' : ''}"
             data-id="${escapeAttr(acc.id)}">
            <span class="account-key">${idx + 1}</span>
            <span class="account-email">${escapeHtml(acc.email || acc.id)}</span>
            <span class="account-provider">${providerIcon(acc.provider)}${pending ? `<span class="account-provider-status">${acc.provider === 'fastmail' ? '· not connected' : '· needs auth'}</span>` : ''}</span>
        </div>
    `;
    }).join('');

    els.accountSelector.querySelectorAll('.account-item').forEach(el => {
        el.addEventListener('click', () => {
            const acc = state.accounts.find(a => a.id === el.dataset.id);
            if (acc) selectAccount(acc);
        });
    });
}

function selectAccount(account) {
    if (account.authStatus === 'pending') {
        // Not authorized yet — every mailbox fetch would fail. Route into
        // the authorize flow instead.
        authorizeAccountFromBanner(account.id);
        return;
    }
    state.currentAccount = account;
    state.mailboxes = [];
    state.emails = [];
    state.threadGroups = new Map();
    state.expandedThreads = new Set();
    state.currentMailbox = null;
    state.currentEmail = null;
    state.selectedIndex = 0;
    state.currentSplit = 'all';
    state.splits = [];
    state.splitCounts = {};
    // Sort order is session-only (kata 09ef), reset to the default on
    // every account switch — same treatment as currentSplit above.
    state.sortOrder = 'date_desc';
    renderSortToggle();
    // splitListCache, emailCache, and scrollPositions are all account-scoped
    // (their keys include state.currentAccount.id). Switching accounts can't
    // surface previous-account state, and returning to an account finds its
    // cache entries intact — no cold reloads for previously-viewed mailboxes
    // or emails.
    // Clear the list pane NOW: if the new account's mailbox fetch is slow
    // (or fails), the previous account's emails must not stay on screen
    // looking like this account's inbox.
    els.emailList.innerHTML = '<div class="loading">Loading</div>';
    lastRenderedContext = null;
    renderAccounts();
    loadMailboxes();
    loadIdentities();
    // Tab sets are per-account now; rebuild the split row (also refreshes
    // counts via loadSplitCounts).
    loadSplits();
}

// Account-scoped cache key. Prefixing every read/write with the active
// account's id keeps emailCache and scrollPositions safe across switches:
// a Gmail id under account "gmail" can't collide with the same string
// under account "outlook-aristotle", and re-selecting an account finds
// its previous cache entries instead of a cold fetch.
function cacheKey(emailId) {
    return (state.currentAccount?.id ?? '') + ':' + emailId;
}

async function loadSplits() {
    const accountId = state.currentAccount?.id;
    try {
        const splits = await api('GET', '/splits');
        if (state.currentAccount?.id !== accountId) return; // stale response guard
        state.splits = splits;
        renderSplitTabs();
        loadSplitCounts();
    } catch (err) {
        // Stale failure guard: a request from the previous account erroring
        // late must not wipe the new account's already-loaded splits.
        if (state.currentAccount?.id !== accountId) return;
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
        // Contact autocomplete (kata e64s): never suggest the user's own
        // send-as addresses back to them. Accumulates across every account
        // visited this session rather than resetting per switch — an
        // address that's "you" on one account is still "you" everywhere.
        for (const id of state.identities) {
            if (id.email) ownIdentityEmails.add(id.email.toLowerCase());
        }
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
             data-split="${escapeAttr(split.id)}" title="Ctrl+${idx + 1}">
            <span class="split-name">${getSplitIcon(split)}${escapeHtml(split.name)}</span>${countBadge}
        </div>
    `;
    }).join('');

    els.splitTabs.querySelectorAll('.split-tab').forEach(el => {
        el.addEventListener('click', () => selectSplit(el.dataset.split));
    });
}

function splitCacheKey() {
    return `${state.currentAccount?.id || ''}:${state.currentMailbox?.id || ''}:${state.currentSplit || 'all'}:${state.starredOnly ? 'S' : ''}:${state.sortOrder}:${getSearchQuery()}`;
}

function invalidateSplitListCache() {
    delete splitListCache[splitCacheKey()];
}

function selectSplit(splitId) {
    state.currentSplit = splitId;
    renderSplitTabs();
    // loadEmails now renders from splitListCache instantly when a hit exists,
    // then refreshes in the background.
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

        // Contact autocomplete (kata e64s): background-fill the index with
        // the Sent mailbox's first page, now that state.mailboxes can
        // resolve it. Fire-and-forget — it self-handles its own failure.
        harvestSentContactsOnce();
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
    url += `&sort=${state.sortOrder}`;
    const search = getSearchQuery();
    if (search) url += `&search=${encodeURIComponent(search)}`;
    return url;
}

// Stale-snapshot revalidation. After a server restart the backend serves the
// previous run's disk-restored list (instant paint) tagged with
// x-supervillain-stale: 1, while its warmer fetches live data in the
// background. Re-poll on a short timer — each poll is a cheap backend cache
// read — until the tag clears. Bounded so a warmer stuck on provider rate
// limits can't keep the poll alive forever.
const STALE_REVALIDATE_MS = 5000;
// The warmer replaces the DEFAULT account's inbox within ~20 s of boot
// (inbox-first, default-account-first ordering), but a non-default
// account's turn comes after every account before it finishes a full
// pass — measured ~2.5 min with four accounts, worst case longer. Each
// poll is a sub-ms local cache read, so the bound is generous.
const STALE_REVALIDATE_MAX = 96; // ≈8 minutes
let staleRevalidateTimer = null;
// Poll budget per list context (splitCacheKey()) — a single global counter
// would let a few stale mailboxes browsed after a restart drain the budget
// for the rest (roborev 307 #4). The whole map is cleared whenever ANY
// context comes back fresh: a stale→fresh transition means the warmer is
// making progress (or the server restarted and re-staled everything), so
// exhausted contexts get re-armed instead of being locked out for the
// page's lifetime — and the map can't accumulate an entry per search
// string browsed (roborev 308 #2).
const staleRevalidateAttempts = new Map();

function scheduleStaleRevalidate(context) {
    const used = staleRevalidateAttempts.get(context) || 0;
    if (used >= STALE_REVALIDATE_MAX) return;
    staleRevalidateAttempts.set(context, used + 1);
    clearTimeout(staleRevalidateTimer);
    staleRevalidateTimer = setTimeout(() => {
        // Only refetch if the user is still looking at the same list.
        if (splitCacheKey() === context) loadEmails();
    }, STALE_REVALIDATE_MS);
}

// Cheap deep-equality for the poll loop: identical payloads (warmer hasn't
// replaced the entry yet) must not re-render — a re-render resets the
// selection to the top row, which would visibly fight the user every poll.
// Server JSON key order is stable, so stringify comparison is exact.
function emailListsEqual(a, b) {
    return a.length === b.length && JSON.stringify(a) === JSON.stringify(b);
}

// Which context's list the pane currently shows (null while a Loading
// placeholder is up). Both render-skips in loadEmails require this to match
// in addition to payload equality — equality alone is a false proxy for
// "already on screen": deep-equal payloads can belong to different contexts
// (two empty mailboxes; a split holding every message vs. 'all'), and
// skipping then would strand whatever the pane last showed (roborev 308 #1).
let lastRenderedContext = null;

async function loadEmails() {
    if (!state.currentMailbox) return;

    // Cancel any in-flight email fetch
    if (loadEmailsController) loadEmailsController.abort();
    loadEmailsController = new AbortController();

    // Snapshot context at request time for stale detection
    const context = splitCacheKey();

    // Superhuman-style: render the cached list immediately so the
    // mailbox/split/account switch feels instant. The network refresh
    // below races in the background and replaces the list when it
    // arrives. Only show the "Loading" placeholder on a true cold miss
    // (no cached entry and no in-memory emails).
    if (splitListCache[context]) {
        // Skip the eager repaint only when the pane already shows exactly
        // this context's list with exactly this payload. During stale-
        // snapshot revalidation every poll tick re-enters loadEmails, and
        // an unconditional repaint here reset the selection to row 0 before
        // the fetch even started (roborev 307 #1). See lastRenderedContext
        // for why payload equality alone isn't sufficient.
        if (lastRenderedContext !== context
            || !emailListsEqual(state.emails, splitListCache[context])) {
            state.emails = [...splitListCache[context]];
            state.selectedIndex = 0;
            rebuildThreadGroups();
            renderEmailList();
        }
    } else if (state.emails.length === 0) {
        els.emailList.innerHTML = '<div class="loading">Loading</div>';
        lastRenderedContext = null;
    }

    try {
        const url = buildEmailListUrl(state.currentMailbox.id);
        const { data: emails, headers } =
            await apiWithMeta('GET', url, null, loadEmailsController.signal);

        // Stale response guard: discard if context changed during fetch
        if (splitCacheKey() !== context) return;

        splitListCache[context] = [...emails];
        // Render unless the pane already shows this context's list with an
        // identical payload. lastRenderedContext is null while a Loading
        // placeholder is up, so a cold miss always renders — even an
        // identical-looking (e.g. empty) list carried over in state.emails.
        if (lastRenderedContext !== context || !emailListsEqual(state.emails, emails)) {
            state.emails = emails;
            state.selectedIndex = 0;
            rebuildThreadGroups();
            renderEmailList();
            harvestContacts(emails, state.currentAccount?.id);
        }

        if (headers.get('x-supervillain-stale') === '1') {
            scheduleStaleRevalidate(context);
        } else {
            staleRevalidateAttempts.clear();
        }
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
            extendThreadGroups(newEmails);
            splitListCache[context] = [...state.emails];
            renderEmailList();
            harvestContacts(newEmails, state.currentAccount?.id);
        }
    } catch (err) {
        console.warn('Refill failed:', err);
    } finally {
        refillInFlight = false;
    }
}

// ============================================================================
// Contact autocomplete on compose To/Cc (kata e64s, task B6)
// ============================================================================
// Client-side only — no Contact/CardDAV API exists, and this version adds no
// server surface at all. state.contactIndex is built purely from mail
// already fetched: (a) every loaded email list page's from/to/cc (hooked
// into loadEmails/maybeRefillEmails above), and (b) one background fetch of
// the Sent mailbox's first page per account session (harvestSentContactsOnce
// below). Ranking is frequency count desc, then lastSeen desc; the account's
// own identity addresses are excluded (see ownIdentityEmails).

// The one account's contact map inside state.contactIndex, created on first
// touch. Per-account isolation follows the selectAccount convention
// (splitListCache/emailCache/scrollPositions): a switch must never surface
// another account's contacts in compose.
function contactIndexFor(accountId) {
    let index = state.contactIndex.get(accountId);
    if (!index) {
        index = new Map();
        state.contactIndex.set(accountId, index);
    }
    return index;
}

// Folds a page of Email objects (list-view shape: {id, from, to, cc,
// receivedAt, ...}) into the harvested account's contact map. Idempotent per
// message id (keyed per account) so re-rendering an already-fetched page —
// e.g. the splitListCache instant-switch snapshot — never inflates counts.
function harvestContacts(emails, accountId) {
    if (!accountId || !emails || !emails.length) return;

    const index = contactIndexFor(accountId);
    for (const email of emails) {
        const msgKey = `${accountId}:${email.id}`;
        if (harvestedMessageIds.has(msgKey)) continue;
        harvestedMessageIds.add(msgKey);

        const seenInEmail = new Set();
        const addrs = [].concat(email.from || [], email.to || [], email.cc || []);
        for (const addr of addrs) {
            const key = addr?.email?.toLowerCase();
            if (!key || seenInEmail.has(key)) continue;
            seenInEmail.add(key);

            const existing = index.get(key);
            if (existing) {
                existing.count += 1;
                if (addr.name) existing.name = addr.name;
                if (email.receivedAt && email.receivedAt > existing.lastSeen) {
                    existing.lastSeen = email.receivedAt;
                }
            } else {
                index.set(key, {
                    email: addr.email,
                    name: addr.name || '',
                    lastSeen: email.receivedAt || '',
                    count: 1,
                });
            }
        }
    }
}

// One-shot per account session: resolves the Sent mailbox from
// state.mailboxes (loadMailboxes must have populated it already) and
// harvests its first ~100 messages. A failure here is not user-facing — it's
// a background enrichment, not a load-bearing fetch — so it degrades
// silently to list-only harvesting via console.warn.
async function harvestSentContactsOnce() {
    const accountId = state.currentAccount?.id;
    if (!accountId || sentHarvestedAccounts.has(accountId)) return;
    sentHarvestedAccounts.add(accountId);

    const sent = state.mailboxes.find(m => m.role === 'sent');
    if (!sent) return;

    try {
        const emails = await api('GET', `/emails?mailbox_id=${sent.id}&limit=100`);
        harvestContacts(emails, accountId);
    } catch (err) {
        console.warn('Contact harvest: Sent mailbox fetch failed:', err);
    }
}

// Comma-segment boundaries around `pos` in a To/Cc field's raw value. Shared
// by the matcher (reads the in-progress segment) and acceptContactAutocomplete
// (replaces it) so both agree on the same span — critical for correctness on
// a mid-field edit (segment isn't necessarily the last one in the field).
function contactSegmentBounds(value, pos) {
    const commaBefore = value.lastIndexOf(',', pos - 1);
    let start = commaBefore === -1 ? 0 : commaBefore + 1;
    while (start < pos && value[start] === ' ') start++;
    let end = value.indexOf(',', pos);
    if (end === -1) end = value.length;
    return [start, end];
}

// Pure rank/match helper: given the in-progress segment text, returns up to 6
// of the CURRENT account's contacts ranked by frequency count desc, then
// lastSeen desc. Matches on email prefix OR name substring, case-insensitive;
// requires 2+ chars.
function rankContactMatches(query) {
    const q = query.trim().toLowerCase();
    if (q.length < 2) return [];

    const index = state.contactIndex.get(state.currentAccount?.id);
    if (!index) return [];

    const matches = [];
    for (const c of index.values()) {
        if (ownIdentityEmails.has(c.email.toLowerCase())) continue;
        const emailMatch = c.email.toLowerCase().startsWith(q);
        const nameMatch = c.name && c.name.toLowerCase().includes(q);
        if (emailMatch || nameMatch) matches.push(c);
    }

    matches.sort((a, b) => (b.count - a.count) || (b.lastSeen || '').localeCompare(a.lastSeen || ''));
    return matches.slice(0, 6);
}

function contactFieldEl(field) {
    return field === 'to' ? els.composeTo : els.composeCc;
}

function contactAcEl(field) {
    return field === 'to' ? els.composeToAutocomplete : els.composeCcAutocomplete;
}

function handleContactFieldInput(field) {
    const input = contactFieldEl(field);
    const pos = input.selectionStart ?? input.value.length;
    const [start] = contactSegmentBounds(input.value, pos);
    const query = input.value.slice(start, pos);

    const matches = rankContactMatches(query);
    if (matches.length === 0) {
        closeContactAutocomplete();
        return;
    }

    contactAcMatches = matches;
    state.contactAcField = field;
    state.contactAcIndex = 0;
    renderContactAutocomplete(field);
}

function renderContactAutocomplete(field) {
    const el = contactAcEl(field);
    el.innerHTML = contactAcMatches.map((c, idx) => `
        <div class="autocomplete-item ${idx === state.contactAcIndex ? 'selected' : ''}" data-index="${idx}">
            <span>${escapeHtml(c.name || c.email)}</span>
            ${c.name ? `<span class="ac-hint">${escapeHtml(c.email)}</span>` : ''}
        </div>
    `).join('');
    el.classList.remove('hidden');

    el.querySelectorAll('.autocomplete-item').forEach(item => {
        item.addEventListener('mousedown', (e) => {
            e.preventDefault(); // keep focus in the input, mirrors search autocomplete
            state.contactAcIndex = parseInt(item.dataset.index);
            acceptContactAutocomplete(field);
        });
    });
}

function renderContactAutocompleteHighlight() {
    if (!state.contactAcField) return;
    contactAcEl(state.contactAcField).querySelectorAll('.autocomplete-item').forEach((item, idx) => {
        item.classList.toggle('selected', idx === state.contactAcIndex);
    });
}

function closeContactAutocomplete() {
    if (!state.contactAcField) return;
    contactAcEl(state.contactAcField).classList.add('hidden');
    state.contactAcField = null;
    contactAcMatches = [];
}

// Replaces the current comma-segment with the selected contact's bare
// address and appends ', ' — but only when completing the field's last
// segment; a mid-field edit leaves whatever already follows untouched so it
// doesn't get duplicated. Only contact.email is ever inserted: To/Cc are
// parsed downstream as plain comma-separated address strings (see
// sendEmail/build_draft_email), so a "Name <email>" form here would ship as
// a literally-invalid recipient.
function acceptContactAutocomplete(field) {
    if (!contactAcMatches.length) return;

    const idx = Math.min(state.contactAcIndex, contactAcMatches.length - 1);
    const contact = contactAcMatches[idx];
    const input = contactFieldEl(field);
    const value = input.value;
    const pos = input.selectionStart ?? value.length;
    const [start, end] = contactSegmentBounds(value, pos);
    const isLastSegment = end >= value.length;

    const before = value.slice(0, start);
    const after = isLastSegment ? '' : value.slice(end);
    const insertion = contact.email + (isLastSegment ? ', ' : '');

    input.value = before + insertion + after;
    const caretPos = (before + insertion).length;
    input.setSelectionRange(caretPos, caretPos);

    closeContactAutocomplete();
    scheduleAutosave();
}

async function loadEmailDetail(emailId) {
    // Drafts restore (kata wm57): in the Drafts mailbox on Fastmail, opening a
    // row edits the draft in compose rather than showing the read-only detail.
    // Gated so every other mailbox — and every other provider — keeps the
    // normal detail open.
    if (state.currentMailbox?.role === 'drafts' && draftsEnabled()) {
        openDraftInCompose(emailId);
        return;
    }

    // Save scroll position of the email we're leaving (if any)
    saveScrollPosition();

    // Use cache if available — render immediately, no await
    const key = cacheKey(emailId);
    if (emailCache[key]) {
        state.currentEmail = emailCache[key];
        renderEmailDetail();
        els.emailBody.scrollTop = scrollPositions[key] || 0;
        showView('detail');
        prefetchAdjacentEmails();

        // Cache-hit opens skip the network GET entirely — prefetchAdjacentEmails
        // fetches with mark_read=false (roborev 302, fix 2) so background
        // warm-up never silently consumes unread state for emails the user
        // hasn't opened. That means the server was never told THIS email is
        // now read; unlike the network-fetch path below (whose GET
        // auto-marks read server-side), we have to ask explicitly.
        // Optimistic, matching toggleUnread: flip the cached email and its
        // list row immediately, without blocking the render above; revert
        // everything alongside showStatus on failure. Split-tab counts are
        // presence counts (compute_split_counts counts every matching email
        // regardless of read state) — only archive/trash/removal changes
        // membership, so mark-read must never adjust them here, same as
        // toggleUnread never does (roborev 303, fix 1).
        const email = state.currentEmail;
        const listItem = state.emails.find(e => e.id === emailId);
        if (email.isUnread) {
            email.isUnread = false;
            if (listItem) listItem.isUnread = false;
            renderEmailList();
            api('POST', `/emails/${emailId}/mark-read`).catch(err => {
                email.isUnread = true;
                if (listItem) listItem.isUnread = true;
                renderEmailList();
                showStatus('Failed to mark read: ' + err.message, 'error');
            });
        }
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
        emailCache[cacheKey(emailId)] = email;
        // The GET above auto-marks the email read server-side, but the
        // response body reflects the pre-mark state — the server fetches
        // the email, then marks it read as a side effect, without mutating
        // the object it already returned. Mirror the flip locally on both
        // the cached object and the matching list row (mirrors mobile
        // renderScreenDetail's network path), or a later cache-hit reopen
        // sees a stale isUnread=true and misfires the mark-read POST above
        // (roborev 303, fix 2).
        // Capture BOTH pre-flip flags: the row can be stale-unread while the
        // server already considers the email read (read on another device
        // after the list loaded) — the response then carries isUnread: false
        // and the row still needs its re-render (roborev 305).
        const wasUnread = email.isUnread || Boolean(listItem?.isUnread);
        email.isUnread = false;
        if (listItem) listItem.isUnread = false;
        // The list row's unread styling only updates on a re-render —
        // returning to the list just toggles CSS classes — so flip it now or
        // the row stays bold until some unrelated action redraws it
        // (roborev 304).
        if (wasUnread) renderEmailList();
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
    // Walk the VISIBLE rows (kata 64z6) so prefetch matches the archive-walk
    // order the user actually advances through.
    const rows = visibleRows();
    const idx = rows.findIndex(r => r.emailId === state.currentEmail?.id);
    if (idx < 0) return;

    // Prefetch next 3 emails (the ones you'll hit when archiving repeatedly).
    // mark_read=false (roborev 302, fix 2): a bare GET auto-marks read
    // server-side, and background warm-up must never silently consume
    // unread state for an email the user hasn't actually opened.
    for (let i = 1; i <= 3; i++) {
        const target = rows[idx + i];
        if (target && !emailCache[cacheKey(target.emailId)]) {
            const key = cacheKey(target.emailId);
            api('GET', `/emails/${target.emailId}?mark_read=false`)
                .then(email => { emailCache[key] = email; })
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

// The compose session a send is currently in flight for, null outside a
// send. runAutosave's mid-send skip is scoped to exactly this session
// (roborev 318): the compose being SENT must not be re-saved (it's about to
// stop being a draft; a late save would ghost a copy in Drafts), but a
// compose the user reopens or starts fresh while a slow send is in flight
// must keep persisting — with a global gate its debounced saves were
// skipped and the leave-flush no-op'd right before clearCompose wiped the
// editor: silent data loss.
let sendingSession = null;

// Lock the compose surface while ITS send is in flight (roborev 321): the
// payload is snapshotted at send initiation (see doSendEmail), so anything
// typed after Ctrl+Enter would be silently discarded — the session-scoped
// gate skips this session's autosaves, the scoped cancel kills its re-armed
// timer, and success clears the editor under a "Sent!" toast. Make mid-send
// edits impossible instead of invisible. Unlocked by the wrapper's finally
// (a failed send must stay editable for retry) and by clearCompose (a new
// or restored compose can start while an old slow send is still in flight
// and must never inherit the lock). readOnly for text fields; disabled for
// controls readOnly doesn't constrain (select/checkbox/datetime/file — a
// disabled file input also ignores the attach handlers' .click()).
function setComposeLocked(locked) {
    for (const el of [els.composeTo, els.composeCc, els.composeSubject,
                      els.composeBody, els.inviteSummary, els.inviteLocation,
                      els.inviteTz]) {
        if (el) el.readOnly = locked;
    }
    for (const el of [els.composeFrom, els.composeInviteEnabled,
                      els.inviteStart, els.inviteEnd, els.composeFileInput]) {
        if (el) el.disabled = locked;
    }
}

// True while the ACTIVE compose is the one a send is in flight for — the
// window where the payload is already snapshotted and any change would be
// silently discarded (or, for attachment removal, silently NOT applied to
// the mail). The DOM lock above can't constrain the non-form entry points —
// dropping a file on the compose view, pasting an image (paste events still
// fire on a readOnly textarea), clicking an attachment's remove button — so
// those handlers check this themselves (roborev 322).
function composeSendLocked() {
    return state.sending && state.composeSession === sendingSession;
}

async function sendEmail() {
    // Re-entry guard: a second Ctrl+Enter while a send is settling its
    // autosave (the awaits at the top of doSendEmail) or in flight must not
    // fire a duplicate POST. state.sending is set here — before the first
    // await — so both rapid presses can't slip past the check.
    if (state.sending) return;
    state.sending = true;
    sendingSession = state.composeSession;
    setComposeLocked(true);
    // Immediate feedback. Everything until the POST settles used to be
    // silent, so a send stalled behind a busy backend read as "nothing
    // happened" — and invited the duplicate Ctrl+Enter guarded above.
    showStatus('Sending…');
    try {
        await doSendEmail();
    } finally {
        state.sending = false;
        sendingSession = null;
        setComposeLocked(false);
    }
}

async function doSendEmail() {
    // A pending autosave firing mid-send would persist a fresh draft of the
    // very mail being sent — un-adopted (compose clears on success) and never
    // deleted. Kill the debounce up front; the session-scoped sending gate
    // (see sendingSession) blocks this compose's new saves from running
    // until the send settles.
    cancelAutosave();
    // Snapshot EVERYTHING the send posts — and the session token its
    // completion gates compare against — synchronously, before the settle
    // await below can yield (roborev 320). That await can block >3s, and the
    // leave paths have no sending gate, so the user can Escape and reopen a
    // different draft before this function resumes; reading the live form or
    // state.composeSession afterward would send the NEW compose's fields,
    // pass the completion gates as their owner, and delete the reopened
    // draft. The token comes from sendingSession (set by the wrapper in this
    // same tick), so it names the compose the user actually hit send on: a
    // stale completion must not clear or navigate a compose it doesn't own —
    // Escape's leave path (flushAutosave/clearCompose/showView) runs freely
    // mid-send, and openDraftInCompose bumps composeSession too (roborev
    // 315).
    const session = sendingSession;

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
    const inReplyTo = state.replyContext?.inReplyTo || null;

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

    // The invite fields join the same pre-settle snapshot; only the POST
    // itself runs after the settle.
    const includeInvite = els.composeInviteEnabled && els.composeInviteEnabled.checked;
    let invite = null;
    if (includeInvite) {
        const summary = els.inviteSummary.value.trim();
        const start = els.inviteStart.value;
        const end = els.inviteEnd.value;
        if (!summary || !start || !end) {
            showStatus('Invite needs title, start, and end', 'error');
            return;
        }
        const tz = (els.inviteTz.value.trim() || state.timezone?.primary || '').trim();
        invite = {
            summary,
            location: els.inviteLocation.value.trim() || null,
            start,
            end,
            tz: tz || null,
            attendees: to.concat(cc).map(email => ({ email })),
        };
    }

    // cancelAutosave() at the top only kills the pending TIMER — a save
    // already in flight keeps running. Without waiting for it, its
    // created/updated id would land after the send-owned draft is already
    // deleted below and never get adopted or removed: a ghost draft
    // (roborev 294, fix 3). doAutosave never rejects, but settle either way
    // defensively.
    if (saveInFlight) await saveInFlight.catch(() => {});
    // The in-flight save above can run >3s; a keystroke during that await
    // fires the input handler's scheduleAutosave() and arms a fresh debounce.
    // But a timer alive NOW may equally belong to a compose the user
    // reopened or started fresh during that await (every leave-compose path
    // flushes the old session's timer first, so a surviving timer is the
    // CURRENT session's) — and since the sending gate is session-scoped
    // (roborev 318), that other compose's save must fire, not die here.
    // Cancel only while this compose is still the one being sent: its
    // re-armed timer would otherwise chain a save landing after the draft
    // delete below (roborev 304; scoped in roborev 319). The sending
    // session's own mid-send saves are skipped at fire time by runAutosave's
    // gate either way. This line is pinned verbatim by the
    // autosave_gate_is_scoped_to_the_sending_session contract test — keep
    // the unbraced single-line form.
    if (state.composeSession === sendingSession) cancelAutosave();
    // The draft this send owns: the ONE capture that must wait for the
    // settle, which adopts an in-flight save's final id (the session-scoped
    // sending gate blocks this compose's new autosaves, so the id can't
    // move again). Deleting the LIVE id at completion instead would ghost
    // the sent mail's draft after a leave (clearCompose nulls it) or
    // destroy an unrelated draft the user opened mid-send (roborev 315).
    // If the user left-and-reopened during the settle this reads the
    // reopened draft's id — the recapture guard below skips that delete.
    const draftId = state.draftId;

    if (invite) {
        try {
            await api('POST', '/calendar/invite', {
                to,
                cc,
                subject,
                body: fullTextBody,
                summary: invite.summary,
                location: invite.location,
                description: null,
                start: invite.start,
                end: invite.end,
                tz: invite.tz,
                attendees: invite.attendees,
                from_address: fromAddress,
                // Roborev 186 #6: pass through attachments so the invite+files
                // combo doesn't silently drop the user's uploads.
                attachments: readyAttachments.length ? readyAttachments : undefined,
            });
            showStatus('Invite sent!', 'success');
            // Delete the autosaved draft of the mail that just went out — by
            // the captured id, even from a stale completion — UNLESS a newer
            // session has recaptured that very id: openDraftInCompose adopts
            // the EXISTING id rather than POSTing a fresh one, so after
            // leave-mid-send + reopen-from-Drafts an unconditional delete
            // would yank the draft from under the active editor and leave
            // trackedDraftId dead (autosaves 404 with only a console.warn;
            // the next leave-compose wipes the only copy). A live-but-
            // already-sent draft is the safer residue (roborev 316). The
            // check reads the tracked pair — which leave-compose never
            // clears — not state.draftId: a snapshot of the live id misses
            // reopen → leave-again before this completion (the live id is
            // nulled by then) and would delete the reopened draft out from
            // under its still-live tracking (roborev 317). Persisting the
            // post-reopen edits themselves is the session-scoped sending
            // gate's job (see sendingSession, roborev 318); this guard then
            // covers the window where such a mid-send save is still in
            // flight and the tracked id hasn't rotated yet. In the normal
            // still-in-compose case trackedDraftSession === session, so the
            // delete fires.
            const reopened = trackedDraftSession !== session && trackedDraftId === draftId;
            if (!reopened) deleteDraftById(draftId);
            if (state.composeSession === session) {
                clearCompose();
                showView('list');
            }
        } catch (err) {
            // Unconditional (never session-gated): a failed send is a lost
            // email and must surface even after the user moved on.
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
            in_reply_to: inReplyTo,
            from_address: fromAddress,
            attachments: readyAttachments.length ? readyAttachments : undefined,
        });
        showStatus('Sent!', 'success');
        // Same shape as the invite path above: captured-id delete unless a
        // newer session recaptured the id (see the comment there),
        // clear/navigate only while this send still owns the compose.
        const reopened = trackedDraftSession !== session && trackedDraftId === draftId;
        if (!reopened) deleteDraftById(draftId);
        if (state.composeSession === session) {
            clearCompose();
            showView('list');
        }
    } catch (err) {
        // Unconditional (never session-gated): a failed send is a lost
        // email and must surface even after the user moved on.
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
                 data-id="${escapeAttr(m.id)}">
                <span>${escapeHtml(m.name)}</span>
                ${m.unreadEmails > 0 ? `<span class="unread-count">${m.unreadEmails}</span>` : ''}
            </div>
        `).join('');
    // 1p0d: m.name / m.id are attacker-controlled (IMAP / shared / delegated
    // mailbox names). escapeHtml the text content; escapeAttr the data-id
    // attribute — escapeHtml alone doesn't encode quotes, so a crafted id
    // could break out of the attribute.

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
    // Cache key already encodes the starred flag, so toggling switches to
    // a different cached slot rather than throwing both away.
    renderStarredItem();
    updateMailboxNameDisplay();
    loadEmails();
    if (state.currentMailbox.role === 'inbox') loadSplitCounts();
}

function renderSortToggle() {
    if (!els.sortToggle) return;
    const isAsc = state.sortOrder === 'date_asc';
    // Gmail's "oldest first" is only oldest-first *within each page* the
    // server fetches (documented server-side, see gmail.rs's
    // apply_sort_order) — a truly-global oldest-first would require
    // buffering and re-sorting the entire mailbox, which we don't do.
    // Fastmail/Outlook sort globally, so the toggle otherwise reads
    // identically across providers even though the guarantee it makes is
    // weaker for Gmail. Flag it in the label/title (roborev 291) rather
    // than silently letting a Gmail user assume global ordering.
    const isGmailPagedAsc = isAsc && state.currentAccount?.provider === 'gmail';
    els.sortToggle.textContent = isAsc
        ? (isGmailPagedAsc ? 'Oldest first (per page)' : 'Oldest first')
        : 'Newest first';
    els.sortToggle.title = isGmailPagedAsc
        ? 'Gmail sorts oldest-first within each fetched page only — a newer message can still appear on an earlier page.'
        : '';
    els.sortToggle.setAttribute('aria-pressed', String(isAsc));
    els.sortToggle.classList.toggle('active', isAsc);
}

function toggleSortOrder() {
    if (!state.currentMailbox) return;
    state.sortOrder = state.sortOrder === 'date_asc' ? 'date_desc' : 'date_asc';
    // Cache key already encodes sort (splitCacheKey), so toggling switches
    // to a different cached slot rather than throwing both away — same
    // pattern as toggleStarredOnly. Split counts aren't order-sensitive,
    // so no loadSplitCounts() call is needed here.
    renderSortToggle();
    loadEmails();
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

// ============================================================================
// Threading / conversation grouping (kata 64z6, task B7) — client-side v1
// ============================================================================
// state.emails stays the flat DATA model (all fetch/sort/filter/splice ops act
// on it unchanged). visibleRows() derives the VIEW model on top of it: a thread
// with 2+ loaded members collapses to ONE row; an expanded thread yields that
// header row plus one indented sub-row per older member. Selection,
// auto-advance and undo re-insert all index into visibleRows() — that single
// seam is what keeps thread-awareness out of every keyboard/action path.

// Full rebuild — called only on a full list replace (loadEmails). Also clears
// the expand set, since the list content is changing under it.
function rebuildThreadGroups() {
    state.threadGroups = new Map();
    state.expandedThreads = new Set();
    extendThreadGroups(state.emails);
}

// Append-time extension (Muratori constraint — no per-render O(n^2)): fold a
// freshly-appended page into the existing groups, in place, at the same site
// pages enter state.emails. Idempotent per id so a re-seen page (splitListCache
// snapshot, refill overlap) never double-registers a member. threadGroups is
// append-only: archived ids are left in place and simply drop out of the LIVE
// present set that visibleRows() recomputes from state.emails.
function extendThreadGroups(emails) {
    if (!emails || !emails.length) return;
    for (const email of emails) {
        const tid = email.threadId;
        if (!tid) continue; // empty threadId => unknown => never grouped
        let ids = state.threadGroups.get(tid);
        if (!ids) {
            ids = [];
            state.threadGroups.set(tid, ids);
        }
        if (!ids.includes(email.id)) ids.push(email.id);
    }
}

// The seam. Derives the ordered visible-row model from state.emails +
// state.threadGroups + state.expandedThreads. Each row:
//   { kind, emailId, email, threadId, unread, starred, count?, expanded? }
//   kind: 'single' (ungrouped/lone), 'thread' (collapsed header, acts on
//   newest), 'member' (an older member exposed while expanded).
// A thread groups only when it has 2+ members STILL PRESENT in state.emails,
// so archiving a member down to one collapses it back to a single row and the
// count badge stays live. The collapsed row sits at the newest member's
// position in the current sort ("group order follows the newest member").
function visibleRows() {
    const emails = state.emails;
    if (!emails.length) return [];

    // One O(n) pass: per grouped thread, collect present members (in list
    // order), the newest member, and aggregate unread/starred flags.
    const groups = new Map(); // tid -> { members:[email], newest, anyUnread, anyStarred }
    for (const email of emails) {
        const tid = email.threadId;
        if (!tid) continue;
        const known = state.threadGroups.get(tid);
        if (!known || known.length < 2) continue; // never grouped
        let g = groups.get(tid);
        if (!g) {
            g = { members: [], newest: email, anyUnread: false, anyStarred: false };
            groups.set(tid, g);
        }
        g.members.push(email);
        if (email.isUnread) g.anyUnread = true;
        if (email.isFlagged) g.anyStarred = true;
        if (new Date(email.receivedAt) > new Date(g.newest.receivedAt)) g.newest = email;
    }

    const rows = [];
    for (const email of emails) {
        const tid = email.threadId;
        const g = tid ? groups.get(tid) : null;
        // Singleton: no threadId, thread not registered, or only one member
        // still loaded/present.
        if (!g || g.members.length < 2) {
            rows.push({ kind: 'single', emailId: email.id, email, threadId: tid || '', unread: email.isUnread, starred: email.isFlagged });
            continue;
        }
        // Grouped: emit the collapsed header once, at the newest member's
        // position; skip the other members (they're folded in, or listed below
        // when expanded).
        if (email !== g.newest) continue;
        const expanded = state.expandedThreads.has(tid);
        rows.push({ kind: 'thread', emailId: g.newest.id, email: g.newest, threadId: tid, count: g.members.length, unread: g.anyUnread, starred: g.anyStarred, expanded });
        if (expanded) {
            for (const m of g.members) {
                if (m === g.newest) continue;
                rows.push({ kind: 'member', emailId: m.id, email: m, threadId: tid, unread: m.isUnread, starred: m.isFlagged });
            }
        }
    }
    return rows;
}

// Resolve the visible-row index that now represents a given email id — used
// after a re-insert (undo) where the flat state.emails index is meaningless
// against the collapsed view. Falls back to the email's collapsed thread
// header when the id is a non-newest member folded into a thread.
function visibleRowIndexForEmailId(id) {
    const rows = visibleRows();
    let idx = rows.findIndex(r => r.emailId === id);
    if (idx >= 0) return idx;
    const email = state.emails.find(e => e.id === id);
    if (email && email.threadId) {
        idx = rows.findIndex(r => r.threadId === email.threadId);
    }
    return idx >= 0 ? idx : 0;
}

// Expand/collapse a thread inline (count-badge click). Keeps selection on the
// thread's header row so j/k resumes from a stable spot.
function toggleThreadExpand(threadId) {
    if (!threadId) return;
    if (state.expandedThreads.has(threadId)) {
        state.expandedThreads.delete(threadId);
    } else {
        state.expandedThreads.add(threadId);
    }
    const rows = visibleRows();
    const headerIdx = rows.findIndex(r => r.kind === 'thread' && r.threadId === threadId);
    if (headerIdx >= 0) state.selectedIndex = headerIdx;
    renderEmailList();
}

function renderInviteChip(email) {
    if (!email.isInviteToMe || email.inviteMethod !== 'REQUEST') return '';

    const status = email.inviteIsUpdated
        ? 'UPDATED'
        : String(email.inviteStatus || 'NEEDS-ACTION').toUpperCase();
    const presentations = {
        'UPDATED':      { label: 'Updated',        cls: 'updated' },
        'ACCEPTED':     { label: 'Accepted',       cls: 'accepted' },
        'TENTATIVE':    { label: 'Tentative',      cls: 'tentative' },
        'DECLINED':     { label: 'Declined',       cls: 'declined' },
        'DELEGATED':    { label: 'Delegated',      cls: 'delegated' },
        'NEEDS-ACTION': { label: 'Needs response', cls: 'needs-action' },
    };
    const presentation = presentations[status] || presentations['NEEDS-ACTION'];
    const accessibleLabel = 'Calendar invite: ' + presentation.label;
    return `<span class="email-invite email-invite--${presentation.cls}" title="${accessibleLabel}" aria-label="${accessibleLabel}">`
        + '<span class="email-invite-icon" aria-hidden="true">📅</span>'
        + `<span class="email-invite-label">${presentation.label}</span></span>`;
}

function renderEmailList() {
    // Every render draws the CURRENT context's state.emails, so the pane
    // now shows that context — including the empty state below.
    lastRenderedContext = splitCacheKey();
    const rows = visibleRows();
    if (!rows.length) {
        els.emailList.innerHTML = '<div class="empty-state">No emails</div>';
        return;
    }

    const showBadge = state.currentSplit === 'all';
    let lastGroup = null;

    els.emailList.innerHTML = rows.map((row, idx) => {
        const email = row.email;
        const from = email.from[0];
        const fromDisplay = from?.name || from?.email || 'Unknown';
        const date = formatDate(email.receivedAt);
        const badge = showBadge ? getRecipientBadge(email) : null;
        const isThread = row.kind === 'thread';
        const isMember = row.kind === 'member';

        // Date dividers skip member sub-rows entirely (kata 64z6 review): a
        // member is older than the header it sits under, so letting it emit a
        // divider would inject one mid-thread — and advancing lastGroup on it
        // would force a duplicate divider for the next non-member row.
        let divider = '';
        if (!isMember) {
            const group = getDateGroup(email.receivedAt);
            if (group !== lastGroup) {
                lastGroup = group;
                divider = `<div class="date-divider"><span class="date-divider-label">${group}</span></div>`;
            }
        }
        const rowClass = `email-row${idx === state.selectedIndex ? ' selected' : ''}${row.unread ? ' unread' : ''}`
            + `${isThread ? ' email-row-thread' : ''}${isMember ? ' email-row-member' : ''}`;
        // Collapsed/expanded thread header carries a clickable count badge; a
        // click on it toggles expansion instead of opening the message.
        const countBadge = isThread
            ? `<span class="email-thread-count${row.expanded ? ' expanded' : ''}" data-thread="${escapeAttr(row.threadId)}" title="${row.expanded ? 'Collapse' : 'Expand'} conversation (${row.count})">${row.expanded ? '▾ ' : ''}${row.count}</span>`
            : '';

        return divider + `
            <div class="${rowClass}"
                 data-id="${email.id}" data-index="${idx}"${isThread ? ` data-thread="${escapeAttr(row.threadId)}"` : ''}>
                <span class="email-flag ${row.starred ? 'flagged' : ''}">${row.starred ? '★' : '☆'}</span>
                ${countBadge}
                <span class="email-from">${escapeHtml(fromDisplay)}</span>
                ${badge ? `<span class="email-recipient-badge">${escapeHtml(badge)}</span>` : ''}
                <span class="email-subject">
                    ${escapeHtml(email.subject)}
                    <span class="email-preview">— ${escapeHtml(email.preview)}</span>
                </span>
                ${email.hasAttachment ? '<span class="email-attachment">📎</span>' : ''}
                ${renderInviteChip(email)}
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
        renderAttachments(e.attachments, e.id, e.account);
    } else {
        els.attachments.classList.add('hidden');
    }

    if (e.htmlBody) {
        // Pass the saved scroll position so the iframe's load handler can
        // restore it once the iframe is sized and the parent pane is
        // scrollable (the parent can't scroll until the iframe has height).
        const key = cacheKey(e.id);
        renderHtmlBodyIframe(els.emailBody, e.htmlBody, { scrollTop: scrollPositions[key] || 0 });
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
    // ArrowDown can increment past the end before re-rendering. Clamp against
    // the filtered set so one item always remains selected when results exist.
    state.commandPaletteIndex = Math.max(
        0,
        Math.min(state.commandPaletteIndex, filtered.length - 1)
    );

    // Commands can include user-controlled split names, account labels, and
    // ids. Escape each value at the final innerHTML boundary so command data
    // is always text/attribute content, never markup (kata fhtz).
    els.commandResults.innerHTML = filtered.map((cmd, idx) => {
        const action = escapeAttr(cmd.action);
        const name = escapeHtml(cmd.name);
        const shortcut = escapeHtml(cmd.shortcut);
        return `
        <div class="command-item ${idx === state.commandPaletteIndex ? 'selected' : ''}"
             data-action="${action}">
            <span>${name}</span>
            <span class="shortcut">${shortcut}</span>
        </div>
    `;
    }).join('');

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
        scrollPositions[cacheKey(state.currentEmail.id)] = els.emailBody.scrollTop;
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
    // No cache wipe: splitListCache keys include (account, mailbox, split,
    // starred, search), so switching mailbox simply changes which entry
    // loadEmails looks up. The cached snapshot for the new mailbox (if any)
    // renders instantly while the network refresh runs in the background.
    updateMailboxNameDisplay();
    renderMailboxes();
    renderSplitTabs();
    updateActiveFilters();
    loadEmails();
    if (mailbox.role === 'inbox') loadSplitCounts();
}

function setMode(mode) {
    const changed = state.mode !== mode;
    state.mode = mode;
    els.modeIndicator.textContent = mode === 'awaiting' ? '-- AWAITING AUTHORIZATION --' : mode.toUpperCase();
    els.modeIndicator.className = mode;
    // Pulse on every real transition — the per-mode colors alone were easy
    // to miss in peripheral vision. The className assignment above already
    // dropped any previous mode-flash; the reflow read restarts the
    // animation when transitions come back-to-back.
    if (changed) {
        void els.modeIndicator.offsetWidth;
        els.modeIndicator.classList.add('mode-flash');
    }
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
                <span class="account-row-provider">${providerIcon(a.provider)}</span>
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
        els.acctAppPassword.value = '';
        els.acctAppPassword.placeholder = 'unchanged (leave blank to keep)';
        els.acctClientSecret.value = '';
        els.acctClientSecret.placeholder = 'unchanged (leave blank to keep)';
        // client-id is not a secret — backend returns it on the existing record.
        els.acctClientId.value = existing.clientId || '';
        els.acctClientId.placeholder = '';
        // Signature isn't a secret either — the backend echoes it back, so
        // (unlike api-token/client-secret) it's safe to prefill for editing.
        els.acctSignature.value = existing.signature || '';
        els.acctDefaultMarker.textContent = existing.isDefault ? 'yes ★' : 'no';
        const pending = existing.authStatus === 'pending';
        els.acctAuthPill.className = 'auth-status-pill ' + (pending ? 'failed' : 'authorized');
        els.acctAuthPill.textContent = pending
            ? (existing.provider === 'fastmail' ? 'NOT CONNECTED' : 'NEEDS AUTH')
            : 'AUTHORIZED';
    } else {
        els.acctName.value = '';
        els.acctUsername.value = '';
        els.acctEmail.value = '';
        els.acctApiToken.value = '';
        els.acctApiToken.placeholder = 'fmu1-...';
        els.acctAppPassword.value = '';
        els.acctAppPassword.placeholder = 'Fastmail app password (CalDAV)';
        els.acctClientId.value = '';
        els.acctClientId.placeholder = '';
        els.acctClientSecret.value = '';
        els.acctClientSecret.placeholder = '';
        els.acctSignature.value = '';
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
            // CalDAV credential (Basic auth), distinct from the JMAP api-token.
            // Empty → server preserves on update; absent/empty both mean "not set".
            'app-password': els.acctAppPassword.value,
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
    // Signature isn't a secret (unlike api-token/client-secret above): the
    // textarea always holds the value to save, so an empty box means "clear
    // the signature", not "leave it unchanged".
    payload.signature = els.acctSignature.value;
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
    const willShow = els.acctConfirmDelete.classList.contains('hidden');
    els.acctConfirmDelete.classList.toggle('hidden');
    // Focus the "no" button (safe-by-default) when the dialog opens so a
    // keyboard user who opened it with 'd' can confirm/cancel with Enter/Space
    // natively — the modal guard in handleSettingsNormalKey swallows other
    // keys but returns without preventDefault, so native button activation and
    // Tab between buttons still work. "no" is the default so a reflexive Enter
    // cancels the destructive delete rather than confirming it (roborev 410).
    if (willShow) {
        const noBtn = els.acctConfirmDelete.querySelector('[data-confirm="no"]');
        if (noBtn) noBtn.focus();
    }
}

async function actuallyDeleteAccount() {
    // Dismiss the confirm dialog immediately on confirm — the delete is
    // now the user's intent, so the modal shouldn't linger. Without this the
    // dialog's `hidden` class stays false and the confirmOpen-based guards
    // (handleSettingsNormalKey's early-return, the hoisted Cmd+K check) leave
    // the settings keyboard dead and the palette unopenable after a confirmed
    // delete (sefy-final/roborev 409). Hide BEFORE the selectedAccountId
    // early-return so dismissal happens even if no account is selected
    // (roborev 410).
    els.acctConfirmDelete.classList.add('hidden');
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
            c['app-password'] = '';
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
        const selected = i === state.wizardProviderIdx;
        r.classList.toggle('focused', selected);
        r.setAttribute('aria-selected', String(selected));
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
const WIZ_ALL_FIELDS = ['client-id', 'client-secret', 'username', 'api-token', 'app-password'];
const WIZ_FIELD_LABELS = {
    'client-id':     'Client ID',
    'client-secret': 'Client secret',
    'username':      'Email',
    'api-token':     'API token',
    'app-password':  'App password',
};
const WIZ_DESCRIPTORS = {
    gmail: {
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
        title: 'Paste your Fastmail credentials',
        blurb: `Fastmail doesn&rsquo;t use OAuth. You need two separate credentials: an <em>API token</em> (JMAP) and an <em>app password</em> (CalDAV/calendar sync) &mdash; Fastmail&rsquo;s API tokens can&rsquo;t be used for CalDAV.`,
        host: null,           // no browser/loopback step
        fields: ['username', 'api-token', 'app-password'],
        placeholders: { username: 'you@fastmail.com', 'api-token': 'fmu1-...', 'app-password': 'Fastmail app password' },
        instructionsHtml: `
            <div class="wiz-why-head">Get your Fastmail credentials (~2&nbsp;min)</div>
            <ol class="wiz-steps">
                <li>Open <a href="https://app.fastmail.com/settings/security/tokens" target="_blank" rel="noopener">Fastmail &rarr; Settings &rarr; Privacy &amp; Security &rarr; API tokens</a> and click <strong>New API token</strong>. Pick <strong>JMAP</strong> as the type and copy the token (Fastmail only shows it once) into <strong>API token</strong> below.</li>
                <li>Open <a href="https://app.fastmail.com/settings/security/passwords" target="_blank" rel="noopener">Fastmail &rarr; Settings &rarr; Privacy &amp; Security &rarr; App passwords</a> and <strong>New app password</strong> with the default <strong>Mail, Contacts &amp; Calendars</strong> scope (CalDAV/CardDAV/IMAP/SMTP). Copy it into <strong>App password</strong> below.</li>
                <li>Enter your email above. Both credentials are stored locally in your mode-0600 config; neither is ever sent anywhere except Fastmail.</li>
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
    setHint('wiz-app-password-hint', cache['app-password']);
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
    document.getElementById('wiz-continue-provider').innerHTML = providerIcon(provider);

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
    document.getElementById('wiz-success-email').textContent = (acct && acct.email) || '(syncing…)';
    document.getElementById('wiz-success-provider').innerHTML = providerIcon(provider);
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
    const bareKey = !e.ctrlKey && !e.metaKey && !e.altKey;
    // Enter is intentionally NOT bareKey-guarded: chorded Enter (Ctrl/Cmd+Enter)
    // reads as submit/advance, matching the app's Ctrl+Enter=send convention
    // (step 2 uses Ctrl+Enter as submit explicitly). Only the bare-letter/digit
    // nav keys (j/k/1/2/3/a/A) must exclude the chord so Ctrl+J/A/1 fall through
    // (sefy-final/roborev 405).

    if (step === 1) {
        if (e.key === 'Escape')                           { closeWizard(); e.preventDefault(); }
        else if (bareKey && (e.key === 'j' || e.key === 'ArrowDown'))  { focusWizProvider(state.wizardProviderIdx + 1); e.preventDefault(); }
        else if (bareKey && (e.key === 'k' || e.key === 'ArrowUp'))    { focusWizProvider(state.wizardProviderIdx - 1); e.preventDefault(); }
        else if (bareKey && (e.key === '1' || e.key === '2' || e.key === '3')) {
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
        else if (bareKey && (e.key === 'a' || e.key === 'A'))         { wizGoTo(1); e.preventDefault(); }
        else if (e.key === 'Escape')                     { closeWizard(); e.preventDefault(); }
    }
}

let statusTimer = null;   // single cancellable handle so an older showStatus
                          // timer can't blank a newer message (kata hp8w)

function showStatus(message, type = 'info') {
    clearTimeout(statusTimer);
    els.statusMessage.textContent = message;
    els.statusMessage.style.color = type === 'error' ? 'var(--danger)' :
                                    type === 'success' ? 'var(--success)' : 'var(--fg-muted)';
    statusTimer = setTimeout(() => {
        els.statusMessage.textContent = '';
    }, 3000);
}

// ---- Deploy-refresh banner (Linear / Monarch Money style) ----
//
// Static assets are compiled into the server binary via include_str!, so a
// merged fix is not live until the server is rebuilt and restarted
// (./scripts/upgrade.sh). The build id (git short sha from build.rs) changes
// on every deploy, so polling /api/build-id detects a deploy without a
// manual Ctrl+Shift+R. When the id changes, show a fixed banner offering a
// one-click hard refresh. The shell routes are served with Cache-Control:
// no-cache, so location.reload() revalidates and pulls the new embedded
// bytes — that IS the hard refresh.

// The build id the page booted with. Read from the <meta name="build-id">
// tag stamped server-side by index_html (the build that served THIS page),
// so the comparison baseline is the build the user is actually running —
// not the first successful poll, which could fail mid-deploy and record a
// post-deploy id as the boot id, hiding the deploy. null until startDeployPoll
// reads the meta tag (e.g. a shell without the meta — older cached HTML).
let knownBuildId = null;
let deployPollTimer = null;

// Re-check interval. A deploy while the tab is open should surface the
// banner within this many seconds. The fetch is a sub-ms local read of a
// tiny plain-text body, so 60s is cheap. Don't make this aggressive — every
// poll is a request, and a long-idle tab polling every second is wasteful.
const DEPLOY_POLL_INTERVAL_MS = 60_000;

async function checkDeploy() {
    // Banner already up — no more work. The interval is cleared in
    // showDeployBanner, but the window-focus handler still calls this; the
    // early return keeps the "polling stops once the banner is up" contract
    // honest (no refetch, no re-show, just a cheap classList check).
    if (els.deployBanner && !els.deployBanner.classList.contains('hidden')) return;
    try {
        // no-store on the endpoint guarantees the browser never hands back a
        // stale id, so a mismatch always means a real deploy happened.
        const id = await fetch('/api/build-id', { cache: 'no-store' }).then(r => r.text());
        if (knownBuildId === null) {
            // Fallback baseline, ONLY when the <meta name="build-id"> tag is
            // absent (an older shell served before this code, or a mid-deploy
            // HTML/JS version split). The meta tag is the preferred baseline
            // because it's the build that served THIS page; this poll-based
            // fallback restores deploy detection for the rest of the session
            // instead of leaving it permanently blind. The mid-deploy-poll
            // race (first poll fails, records a post-deploy id as the boot
            // id) only exists in this fallback path — the meta path is
            // deterministic, so this trade-off is confined to the no-meta
            // case.
            knownBuildId = id;
            return;
        }
        if (id !== knownBuildId) {
            showDeployBanner();
        }
    } catch (_) {
        // A transient fetch failure (server mid-restart, network blip) is
        // expected during a deploy — the next tick retries. Never surface the
        // banner on a failure: a network error is not evidence of a new
        // version, and a false banner that reloads into a still-restarting
        // server would show a blank page.
    }
}

function showDeployBanner() {
    if (!els.deployBanner) return;
    els.deployBanner.classList.remove('hidden');
    // Stop polling once the banner is up — the user has been notified, no
    // point in re-fetching the build id every minute until they click.
    if (deployPollTimer) {
        clearInterval(deployPollTimer);
        deployPollTimer = null;
    }
}

// The Refresh button's click handler. location.reload() is a hard refresh
// here because the shell routes carry Cache-Control: no-cache — the browser
// revalidates and, since the server's embedded bytes changed, pulls the new
// app.js / style.css / index.html instead of serving a stale heuristic
// cache copy.
function hardRefresh() {
    location.reload();
}

function startDeployPoll() {
    if (!els.deployBanner) return; // shell without the banner element — skip
    // Capture the build id this page actually booted with, stamped server-side
    // in <meta name="build-id">. This is the comparison baseline — NOT the
    // first successful poll (which could fail mid-deploy and record a
    // post-deploy id as the boot id, hiding the deploy). Absent on an older
    // cached shell; knownBuildId stays null and checkDeploy falls back to
    // adopting the first successful poll as the baseline (see checkDeploy) so
    // a no-meta session isn't permanently blind to deploys.
    const meta = document.querySelector('meta[name="build-id"]');
    if (meta) knownBuildId = meta.content;
    // Wire the Refresh button here, after the deployBanner guard, so a
    // mixed-version shell (stale index.html with no #deploy-refresh-btn +
    // fresh app.js) can't throw a TypeError here and abort the rest of
    // init(). The guard above already proved #deploy-banner exists; belt-
    // and-suspenders the button too in case the HTML has the banner div but
    // not the button.
    if (els.deployRefreshBtn) {
        els.deployRefreshBtn.addEventListener('click', hardRefresh);
    }
    checkDeploy();
    deployPollTimer = setInterval(checkDeploy, DEPLOY_POLL_INTERVAL_MS);
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

    // Command palette shortcut (kata sefy): hoisted ABOVE the per-view
    // early returns so Cmd+K reaches the palette from every screen —
    // settings (wizard/insert/normal) and compose insert included — not
    // just list/detail normal mode. The overlay checks above (help,
    // palette, search, split modal) still take precedence, so Cmd+K never
    // opens over an already-open overlay. Without this hoist the settings
    // and compose branches of commandsForView were unreachable at runtime
    // and the palette's show/hide invariant silently failed (roborev 378 #1/#2).
    if ((e.metaKey || e.ctrlKey) && !e.shiftKey && e.key.toLowerCase() === 'k' && els.acctConfirmDelete.classList.contains('hidden')) {
        openCommandPalette();
        e.preventDefault();
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
        // Contact autocomplete (kata e64s): only intercept when the dropdown
        // is actually open AND the event target is the To/Cc input it
        // belongs to. Gating on both means a closed dropdown never eats a
        // key, and the block below (Escape / Ctrl+Enter / Ctrl+Shift+A) stays
        // reachable exactly as before everywhere else in compose.
        if (state.contactAcField && (e.target === els.composeTo || e.target === els.composeCc)) {
            if (e.key === 'Escape') {
                closeContactAutocomplete();
                e.preventDefault();
                return;
            } else if (e.key === 'ArrowDown') {
                state.contactAcIndex = Math.min(state.contactAcIndex + 1, contactAcMatches.length - 1);
                renderContactAutocompleteHighlight();
                e.preventDefault();
                return;
            } else if (e.key === 'ArrowUp') {
                state.contactAcIndex = Math.max(0, state.contactAcIndex - 1);
                renderContactAutocompleteHighlight();
                e.preventDefault();
                return;
            } else if (e.key === 'Enter' || e.key === 'Tab') {
                acceptContactAutocomplete(state.contactAcField);
                e.preventDefault();
                return;
            }
        }

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

    // Compose normal-mode: Ctrl+Enter still sends. Escape blurs the field
    // (normal mode) but the mail on screen is unmistakably what the user
    // means to send — without this branch the chord silently fell through
    // to the global handler and did nothing, which read as "send is broken".
    if (state.view === 'compose' && state.mode === 'normal' && e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
        sendEmail();
        e.preventDefault();
        return;
    }

    // Compose normal-mode single-letter bindings. Bare keypresses only —
    // Ctrl/Cmd/Alt chords (select-all, italics, devtools) must fall through
    // to the browser, not be swallowed as vim commands (roborev 312 #3).
    const bareKey = !e.ctrlKey && !e.metaKey && !e.altKey;

    // 'a' opens the file picker instead of reply-all
    if (state.view === 'compose' && state.mode === 'normal' && bareKey && e.key === 'a') {
        els.composeFileInput.click();
        e.preventDefault();
        return;
    }

    // 'i' re-enters insert in the message body — the vim counterpart to
    // Escape. Focusing triggers the field's focus listener, which flips
    // the mode (and the indicator) to insert.
    if (state.view === 'compose' && state.mode === 'normal' && bareKey && e.key === 'i') {
        els.composeBody.focus();
        e.preventDefault();
        return;
    }

    // Ctrl+1-9: jump to split tab
    if (e.ctrlKey && e.key >= '1' && e.key <= '9') {
        selectSplitByIndex(parseInt(e.key) - 1);
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
    const confirmOpen = !els.acctConfirmDelete.classList.contains('hidden');
    if (key === 'Escape') {
        if (confirmOpen) {
            els.acctConfirmDelete.classList.add('hidden');
            return;
        }
        closeSettings();
        return;
    }
    // While the delete-confirm dialog is modal, ignore everything but Escape
    // (handled above) and Tab (focus-trapped below) so j/k/d/D/Enter don't act
    // on the account list underneath (sefy-final/roborev 398).
    if (confirmOpen) {
        // Focus trap: Tab/Shift+Tab cycles between the yes/no buttons so focus
        // can't escape into the settings form underneath the modal. The guard
        // returns without preventDefault for other keys so native button
        // activation (Enter/Space on the focused button) still works
        // (sefy-final/roborev 411).
        if (e.key === 'Tab') {
            e.preventDefault();
            const buttons = Array.from(els.acctConfirmDelete.querySelectorAll('button'));
            if (buttons.length) {
                const idx = buttons.findIndex(b => b === document.activeElement);
                if (idx === -1) {
                    // Focus escaped the dialog (or no button was focused) —
                    // restore safe-by-default: focus "no" so a reflexive Enter
                    // cancels, not confirms, the destructive delete (roborev 412).
                    const noBtn = els.acctConfirmDelete.querySelector('[data-confirm="no"]');
                    if (noBtn) noBtn.focus();
                } else {
                    buttons[(idx + (e.shiftKey ? -1 : 1) + buttons.length) % buttons.length].focus();
                }
            }
        }
        return;
    }
    // Bare-key matches only — Ctrl/Cmd/Alt chords (Ctrl+A select-all, Ctrl+D
    // bookmark, Ctrl+J/K, Ctrl+Enter) must fall through to the browser, not
    // fire settings actions (mirrors the wizard bareKey guard).
    const bareKey = !e.ctrlKey && !e.metaKey && !e.altKey;
    if (bareKey && key === 'a') {
        beginAddAccount();
        e.preventDefault();
        return;
    }
    if (state.selectedAccountId) {
        if (bareKey && key === 'd') {
            toggleConfirmDelete();
            return;
        }
        if (bareKey && key === 'D') {
            setDefaultAccount(state.selectedAccountId);
            return;
        }
        if (bareKey && key === 'Enter') {
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
    if (bareKey && (key === 'j' || key === 'k')) {
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
                // Cancel-with-keep: persist the last edits, then leave the
                // draft saved on the server (kata wm57). clearCompose stops
                // tracking it without deleting.
                escapeCompose();
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
        case 'm':
            if (state.view === 'detail' && state.currentEmail?.calendarEvent) {
                rsvpToEvent('TENTATIVE');
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
        closeCommandPalette({ cancelled: true });
        e.preventDefault();
    } else if (e.key === 'Enter') {
        const selected = els.commandResults.querySelector('.command-item.selected');
        if (selected) {
            executeCommand(selected.dataset.action);
            closeCommandPalette();
        } else {
            closeCommandPalette({ cancelled: true });
        }
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
    // Navigation indexes the VISIBLE row model (kata 64z6): a collapsed thread
    // is one step; an expanded thread's members are individual steps.
    const rows = visibleRows();
    const newIndex = state.selectedIndex + delta;
    if (newIndex < 0 || newIndex >= rows.length) return;

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
        const row = rows[state.selectedIndex];
        if (row) loadEmailDetail(row.emailId);
    }
}

function moveToTop() {
    state.selectedIndex = 0;
    renderEmailList();
}

function moveToBottom() {
    state.selectedIndex = Math.max(0, visibleRows().length - 1);
    renderEmailList();
}

function openSelected() {
    // Enter/o opens the selected visible row: a collapsed thread opens its
    // NEWEST message (kata 64z6 — same as clicking the row body; expansion is
    // the count-badge affordance, not Enter).
    const row = visibleRows()[state.selectedIndex];
    if (row) {
        loadEmailDetail(row.emailId);
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
    // Acting on a collapsed thread row acts on the NEWEST message only (kata
    // 64z6, v1 — no bulk thread actions): the visible row already carries it.
    const row = visibleRows()[state.selectedIndex];
    return row?.emailId;
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
    // emailAction already removed the current email from state.emails, so the
    // next VISIBLE row at the same index is the one to advance to (kata 64z6 —
    // auto-advance walks visibleRows, not the flat list, so a collapsed thread
    // is one stop).
    const rows = visibleRows();
    if (rows.length === 0) {
        showView('list');
        maybeRefillEmails();
        return;
    }

    const nextIndex = Math.min(state.selectedIndex, rows.length - 1);
    state.selectedIndex = nextIndex;
    const nextRow = rows[nextIndex];

    if (nextRow) {
        loadEmailDetail(nextRow.emailId);
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

        showStatus(`Archived ${result.archived} emails from ${result.sender}.`, 'success');
        loadSplitCounts(); // resync with server truth
        maybeRefillEmails();
    } catch (err) {
        // Revert: re-insert the removed emails
        if (removedEmails.length > 0) {
            state.emails = state.emails.concat(removedEmails);
            // Re-sort respecting the active sort order (kata review
            // follow-up) — a hardcoded descending re-sort here would scramble
            // the list under date_asc instead of restoring it.
            const dir = state.sortOrder === 'date_asc' ? 1 : -1;
            state.emails.sort((a, b) => dir * (new Date(a.receivedAt) - new Date(b.receivedAt)));
            // Re-registration is idempotent (ids were never pruned from the
            // append-only groups), but keep it explicit for the revert path.
            extendThreadGroups(removedEmails);
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
    // threadGroups is append-only — the removed ids just drop out of the live
    // present set visibleRows() recomputes. Clamp selection against the VISIBLE
    // row count (kata 64z6), which may differ from state.emails.length once a
    // thread has collapsed.
    const visibleCount = visibleRows().length;
    if (state.selectedIndex >= visibleCount) {
        state.selectedIndex = Math.max(0, visibleCount - 1);
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
    // From the list, reply/forward targets the selected visible row's message
    // — a collapsed thread yields its newest (kata 64z6).
    return state.view === 'detail' ? state.currentEmail : visibleRows()[state.selectedIndex]?.email;
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

    const quotedHtml = email.htmlBody || null;
    const quotedText = email.htmlBody
        ? htmlToPlainText(email.htmlBody)
        : (email.textBody || '');

    state.replyContext = {
        inReplyTo: email.id,
        quotedHtml,
        quotedText,
    };

    autoSelectFromAddress(email);

    // escapeHtml the From name/email: renderComposeQuote assigns this header
    // to innerHTML, and from?.name is attacker-controlled (a display name like
    // <img src=x onerror=...> would execute on Reply). startForward already
    // escapes the same field; this matches it (kata hp8w).
    const header = `On ${formatDate(email.receivedAt)}, ${escapeHtml(from?.name || from?.email || '')} wrote:`;
    renderComposeQuote(header, quotedHtml, quotedText);

    showView('compose');
}

function startForward() {
    const email = getComposeEmail();
    if (!email) return;

    clearCompose();
    autoSelectFromAddress(email);

    els.composeSubject.value = email.subject.startsWith('Fwd:') ? email.subject : `Fwd: ${email.subject}`;

    const from = email.from[0];
    const quotedHtml = email.htmlBody || null;
    const quotedText = email.htmlBody
        ? htmlToPlainText(email.htmlBody)
        : (email.textBody || '');

    state.replyContext = { quotedHtml, quotedText };

    const headerLines = `---------- Forwarded message ---------<br>From: ${escapeHtml(from?.name || '')} &lt;${escapeHtml(from?.email)}&gt;<br>Subject: ${escapeHtml(email.subject)}`;
    renderComposeQuote(headerLines, quotedHtml, quotedText);

    showView('compose');
}

// Per-account plain-text signature, prefilled into a fresh compose body.
// clearCompose is the single choke point for new/reply/forward (all three
// call it before anything else), so prefilling here covers all of them
// uniformly. Never re-injected at send time — sendEmail sends exactly
// what's left in the textarea after the user edits/deletes freely. The
// account is per-account, not per-identity: switching compose-from does
// NOT swap this (accounts can't be switched mid-compose anyway).
function composeSignaturePrefill() {
    const sig = state.currentAccount?.signature;
    return sig ? `\n\n-- \n${sig}` : '';
}

// The compose discard path (kata sefy): flush the last edits to the draft,
// stop tracking it without deleting, and return to the list. This is the
// exact sequence the Escape/'q' keybinding runs in handleNormalModeKey —
// extracted so the command palette's 'Close Draft' action calls the SAME
// function the keybinding calls rather than duplicating the three steps.
function escapeCompose() {
    flushAutosave();
    clearCompose();
    showView('list');
}

function clearCompose() {
    // Every fresh (or restored) compose is a new autosave session: bump the
    // token so a still-in-flight save from the previous draft can't adopt its
    // id here, and cancel any pending debounce. draftId is nulled — a restore
    // sets it again after this runs; a plain new compose leaves it null until
    // the first autosave POSTs.
    state.composeSession++;
    cancelAutosave();
    // A fresh compose must never inherit a still-in-flight send's lock
    // (roborev 321) — that send's finally will unlock again harmlessly.
    setComposeLocked(false);
    state.draftId = null;
    els.composeTo.value = '';
    els.composeCc.value = '';
    // A dropdown left open from the previous compose session must not linger
    // (kata e64s) — same element, so a stale state.contactAcField would
    // otherwise still gate keydown handling on the new session's first
    // keystroke.
    closeContactAutocomplete();
    els.composeSubject.value = '';
    els.composeBody.value = composeSignaturePrefill();
    // Dirty-check baseline: composeDirty compares the body against this exact
    // string, so an untouched signature prefill isn't autosaved as a draft.
    state.composeBaseline = els.composeBody.value;
    els.composeBody.setSelectionRange(0, 0);
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

// ============================================================================
// Draft autosave / restore (kata wm57) — Fastmail-only, plain-text
// ============================================================================
// A debounced background save keeps the compose persisted as a real Drafts
// message: the first save POSTs (adopting the returned id into state.draftId),
// each edit after re-saves as a PUT. Because JMAP can't patch an email body in
// place, the server destroys+recreates on PUT and returns a NEW id, which we
// re-adopt. Autosave never fires on a pristine compose (composeDirty gate) and
// is silent on failure (console.warn only). Sending or explicitly discarding
// deletes the tracked draft; backing out keeps it — that's the feature.

const AUTOSAVE_DEBOUNCE_MS = 3000;
let autosaveTimer = null;
// The promise of the most recently scheduled autosave request (roborev 294,
// fixes 3+4). Every runAutosave() call chains onto this instead of firing its
// own request directly: two saves that would otherwise overlap now run
// strictly one after another, so the second always sees the first's adopted
// draftId rather than racing it and double-POSTing a create. It also gives
// sendEmail a handle to await — cancelAutosave() only kills the pending
// debounce TIMER, not a save whose request is already in flight, so without
// this a late-landing save's created id would never be adopted or deleted,
// orphaning a ghost draft.
let saveInFlight = null;

// Autosave-owned identity for the draft the save chain is currently tracking,
// tagged with the composeSession it belongs to. Deliberately separate from
// state.draftId/state.composeSession (review follow-up): every leave-compose
// path calls flushAutosave() immediately followed by clearCompose(), both
// synchronously — clearCompose nulls state.draftId and bumps composeSession
// before the flushed save's queued `.then()` callback (in doAutosave) ever
// gets a turn to run. If that callback read state.draftId directly at that
// point it would see null and POST a brand-new draft instead of PUTting the
// one being left, duplicating it. trackedDraftId/trackedDraftSession are
// written only by doAutosave (on a successful save) and openDraftInCompose
// (on restore) — clearCompose never touches them — so a queued save always
// PUTs against the id that was live when it was scheduled, regardless of
// what runs after flushAutosave() returns. The session tag is what
// invalidates a stale id for a *later* compose (see doAutosave below)
// instead of a synchronous null-out. composeSession only ever increments
// (see state.composeSession++ at clearCompose/openDraftInCompose), so
// doAutosave's write to these two is additionally guarded on
// `session >= trackedDraftSession`: without that, an older session's save
// completing after a newer restore has already seeded trackedDraftId/
// trackedDraftSession would clobber that seed, and the restored draft's
// next autosave would then session-mismatch and POST a duplicate instead of
// PUTting the restored id.
let trackedDraftId = null;
let trackedDraftSession = -1;

function draftsEnabled() {
    return state.currentAccount?.provider === 'fastmail';
}

// True when the compose holds anything worth saving. Mirrors the send-time
// notion of "has content": any recipient/subject, body diverging from the
// prefill baseline, or a pending attachment (attachments aren't persisted, but
// their presence still marks the compose as a real draft).
function composeDirty() {
    return !!(els.composeTo.value.trim()
        || els.composeCc.value.trim()
        || els.composeSubject.value.trim()
        || els.composeBody.value !== state.composeBaseline
        || state.pendingAttachments.length);
}

function cancelAutosave() {
    if (autosaveTimer) { clearTimeout(autosaveTimer); autosaveTimer = null; }
}

function scheduleAutosave() {
    if (!draftsEnabled()) return;
    cancelAutosave();
    autosaveTimer = setTimeout(runAutosave, AUTOSAVE_DEBOUNCE_MS);
}

// Immediately fire any pending autosave (used when leaving compose so the last
// few seconds of typing aren't lost). No-op when nothing is pending.
//
// Microtask ordering (review follow-up): every leave-compose call site runs
// this then clearCompose() back to back, synchronously — e.g. the Escape
// handler does `flushAutosave(); clearCompose();` with nothing awaited in
// between. runAutosave()'s synchronous prologue (session/payload capture,
// chaining onto saveInFlight) fully completes before control returns here and
// clearCompose() runs, but the chained doAutosave() call itself is a
// microtask that only fires afterward — by then clearCompose has already
// nulled state.draftId and bumped composeSession. doAutosave reads the id to
// save against from trackedDraftId/trackedDraftSession (module state
// clearCompose never touches) rather than state.draftId, precisely so this
// ordering can't turn a "save the last edits" flush into a duplicate-POST.
function flushAutosave() {
    if (!autosaveTimer) return;
    cancelAutosave();
    runAutosave();
}

async function runAutosave() {
    autosaveTimer = null;
    // Never save the compose that is mid-send — the mail is about to stop
    // being a draft, and a save landing now would leave a ghost copy in
    // Drafts. Scoped to the sending session (roborev 318): a different
    // compose (reopened or new mid-send) must keep saving normally, or its
    // edits are silently lost at leave time.
    if (!draftsEnabled() || !composeDirty() || composeSendLocked()) return;
    // Capture everything synchronously: the caller may clear the compose right
    // after (flush-on-leave) while the request is still in flight.
    const session = state.composeSession;
    const payload = {
        to: els.composeTo.value.split(',').map(s => s.trim()).filter(Boolean),
        cc: els.composeCc.value.split(',').map(s => s.trim()).filter(Boolean),
        subject: els.composeSubject.value,
        body: els.composeBody.value,
        in_reply_to: state.replyContext?.inReplyTo || null,
        from_address: els.composeFrom?.value || null,
    };
    // Chain onto whatever save is already running (roborev 294, fix 4) rather
    // than firing this one immediately: if the previous save hasn't adopted
    // its id yet, running concurrently would race it — both would see the
    // same (stale) state.draftId and could both POST a create instead of one
    // PUTting the update. Chaining guarantees this one only starts once the
    // prior save (and its id adoption) has fully settled.
    const previous = saveInFlight || Promise.resolve();
    saveInFlight = previous.then(() => doAutosave(session, payload));
    await saveInFlight;
}

async function doAutosave(session, payload) {
    // Only reuse the tracked id if it was left by this exact compose session
    // — a mismatch means clearCompose already moved on to a new session (the
    // leave-path flush case documented at flushAutosave), so this save must
    // create a fresh draft rather than resurrecting — and overwriting —
    // whatever the old session was tracking.
    const draftId = trackedDraftSession === session ? trackedDraftId : null;
    try {
        const res = draftId
            ? await api('PUT', `/drafts/${encodeURIComponent(draftId)}`, payload)
            : await api('POST', '/drafts', payload);
        // Guard against a stale (older-session) completion clobbering a
        // newer restore's seed: sessions only increase, so `session <
        // trackedDraftSession` means a later openDraftInCompose already
        // wrote a fresher id/session while this save was in flight. Adopting
        // this one's id anyway would make the restored draft's next autosave
        // session-mismatch and POST a duplicate instead of PUTting it.
        if (res?.id && session >= trackedDraftSession) {
            trackedDraftId = res.id;
            trackedDraftSession = session;
        } else if (res?.id && !draftId) {
            // roborev 299 (reverts roborev 298 #3): this completion was
            // rejected above (a later openDraftInCompose already tracked a
            // fresher id/session while this save was in flight), and this
            // save POSTed a brand-new draft — so the server now holds a
            // stray draft nothing points to. Deliberately NOT deleted: this
            // branch is only reachable when the save carried real user
            // content (the composeDirty gate means autosave never fires on a
            // pristine compose), so that "orphan" is a real Drafts message
            // holding the abandoned compose's final edits, stored nowhere
            // else. Destroying the only copy of the user's text in a timing
            // race is strictly worse than the stray-but-real, visible,
            // recoverable draft it would tidy away — never delete user
            // content to clean up client tracking state.
        }
        // Adopt the (possibly changed) id into the visible state only while
        // this is still the active compose — a newer draft must not inherit
        // this save's id.
        if (state.composeSession === session && res?.id) {
            adoptDraftId(res.id);
            showStatus('Draft saved', 'info');
        }
    } catch (err) {
        console.warn('Autosave failed:', err);
    }
}

// Adopt a freshly (re)created draft id. The server destroys+recreates on
// every update, so the tracked id rotates on almost every save — if the OLD
// id is still sitting in the Drafts list or email cache, it's left pointing
// at a now-destroyed message: tapping that row later fetches the dead id and
// errors until a manual reload (roborev 294, fix 2). Swap it in place
// instead. Gated on Drafts actually being the mailbox in view — the id
// rotation is only meaningful for that list/cache; elsewhere there's nothing
// of this draft's to swap.
function adoptDraftId(newId) {
    const oldId = state.draftId;
    if (oldId && oldId !== newId && state.currentMailbox?.role === 'drafts') {
        const row = state.emails.find(e => e.id === oldId);
        if (row) {
            row.id = newId;
            renderEmailList();
        }
        const oldKey = cacheKey(oldId);
        const cached = emailCache[oldKey];
        delete emailCache[oldKey];
        if (cached) {
            cached.id = newId;
            emailCache[cacheKey(newId)] = cached;
        }
    }
    state.draftId = newId;
}

// Fire-and-forget delete of a draft by an id the caller owns (doSendEmail
// captures it before its await). Never reads live state.draftId: at
// completion time that may already belong to a different compose, and
// deleting it would destroy user content (roborev 315). Deliberately
// doesn't touch state.draftId or the autosave timer either — a stale
// completion must not disturb whatever compose is now active; the owning
// path (clearCompose) does that cleanup itself.
function deleteDraftById(id) {
    if (!id || !draftsEnabled()) return;
    api('DELETE', `/drafts/${encodeURIComponent(id)}`)
        .catch(err => console.warn('Draft delete failed:', err));
}

// Restore: open a Drafts-mailbox message in compose (prefilled) instead of the
// read-only detail view, tracking its id so autosave updates it and send
// deletes it. Plain text only — the reply/forward quote context is not
// reconstructed (the draft body is whatever plain text was saved).
async function openDraftInCompose(emailId) {
    let draft = emailCache[cacheKey(emailId)];
    if (!draft || draft.textBody === undefined) {
        try {
            draft = await api('GET', `/emails/${emailId}`);
            emailCache[cacheKey(emailId)] = draft;
        } catch (err) {
            showStatus('Failed to load draft: ' + err.message, 'error');
            return;
        }
    }
    clearCompose();
    // Rehydrate threading (review follow-up): the draft persisted its
    // in_reply_to, so restoring must carry it back into replyContext or every
    // subsequent save/send would silently drop the threading headers. The
    // quote context stays unreconstructed — body text only (documented v1).
    state.replyContext = draft.inReplyTo
        ? { inReplyTo: draft.inReplyTo, quotedHtml: null, quotedText: null }
        : null;
    els.composeTo.value = (draft.to || []).map(t => t.email).filter(Boolean).join(', ');
    els.composeCc.value = (draft.cc || []).map(t => t.email).filter(Boolean).join(', ');
    els.composeSubject.value = draft.subject || '';
    els.composeBody.value = draft.textBody || (draft.htmlBody ? htmlToPlainText(draft.htmlBody) : '');
    const fromEmail = draft.from?.[0]?.email;
    if (els.composeFrom && fromEmail && state.identities.some(i => i.email === fromEmail)) {
        els.composeFrom.value = fromEmail;
    }
    // Track the existing draft and baseline the restored body so simply
    // opening it (no edit) doesn't trigger a redundant save. Seed the
    // autosave module's own tracked id/session too (see trackedDraftId
    // above) — doAutosave reads those, not state.draftId, so without this an
    // edit-then-leave on a restored draft would autosave as a fresh POST
    // instead of a PUT against the restored id.
    state.draftId = emailId;
    trackedDraftId = emailId;
    trackedDraftSession = state.composeSession;
    state.composeBaseline = els.composeBody.value;
    showView('compose');
}

let attachmentIdCounter = 0;

function handleFileSelect() {
    const files = els.composeFileInput.files;
    if (!files.length) return;
    addFiles(files);
    els.composeFileInput.value = '';
}

// HTTP headers are byte strings, so assigning a Unicode File.name directly
// throws for Japanese/emoji and turns Latin-1 into bytes HeaderValue::to_str
// rejects.  Send one ASCII-only RFC 5987 extended value; the upload handler
// decodes it back to UTF-8 before returning the attachment metadata.
function encodeFilenameHeader(filename) {
    return "UTF-8''" + encodeURIComponent(filename).replace(/['()*]/g, char =>
        '%' + char.charCodeAt(0).toString(16).toUpperCase());
}

function uploadAttachment(file, id, controller) {
    const xhr = new XMLHttpRequest();
    xhr.open('POST', '/api/upload');
    xhr.setRequestHeader('Content-Type', file.type || 'application/octet-stream');
    xhr.setRequestHeader('X-Filename', encodeFilenameHeader(file.name));

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
            // The server-decoded value is authoritative for both the compose
            // display and the later send payload.
            att.name = data.name || att.name;
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
    // Mid-send removal is the inverse illusion of a mid-send add: the
    // snapshotted send still carries the attachment (roborev 322).
    if (composeSendLocked()) {
        showStatus('Sending — attachments can no longer be changed', 'error');
        return;
    }
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
    // Single choke point for every attachment add path (file input, drop,
    // paste). A file added mid-send would upload and render, then be
    // aborted by the success path's clearCompose under the "Sent!" toast —
    // the POST carries the pre-settle snapshot (roborev 322). Refuse
    // loudly; a silent no-op would just be the same discard, earlier.
    if (composeSendLocked()) {
        showStatus('Sending — attachments can no longer be changed', 'error');
        return;
    }
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
    commandPalettePreviousFocus = document.activeElement;
    commandPalettePreviousMode = state.mode;
    els.commandPalette.classList.remove('hidden');
    els.commandInput.value = '';
    state.commandPaletteIndex = 0;
    renderCommandPalette();
    els.commandInput.focus();
    setMode('command');
}

function closeCommandPalette({ cancelled = false } = {}) {
    els.commandPalette.classList.add('hidden');

    if (cancelled) {
        const previousFocus = commandPalettePreviousFocus;
        const previousMode = commandPalettePreviousMode;
        commandPalettePreviousFocus = null;
        commandPalettePreviousMode = 'normal';
        let focusRestored = false;
        if (previousFocus?.isConnected) {
            previousFocus.focus();
            focusRestored = document.activeElement === previousFocus;
        }
        // Native focus listeners restore compose/wizard insert mode. Dense
        // settings fields have no focus listener, so use the captured mode as
        // a fallback only when focus was really restored. A disconnected field
        // must not leave body focus trapped in insert mode.
        if (state.mode === 'command') setMode(focusRestored ? previousMode : 'normal');
        return;
    }

    commandPalettePreviousFocus = null;
    commandPalettePreviousMode = 'normal';
    // Only fall back to normal if the executed command didn't set its own
    // mode — e.g. 'search' sets 'search', 'compose' ends in 'insert' via
    // the compose-field focus listener. Unconditionally resetting to
    // 'normal' clobbered openSearch's 'search' mode (the mode indicator
    // read NORMAL while the search bar was open) and any future mode-gated
    // logic would misfire on the palette path (roborev 378 #4).
    if (state.mode === 'command') setMode('normal');
}

// Context-aware command palette (kata sefy, Superhuman Rule #5: make
// commands contextually relevant). One switch on state.view decides what
// shows and in what order — the same field the keydown handler already
// switches on — so the palette's show/hide agrees with the keydown gates
// and never offers an action that then no-ops. No registry, no enum;
// state.view already exists and already drives per-view key handling.
function commandsForView(view) {
    switch (view) {
        case 'detail': {
            // View-native first: the actions that only make sense on an open
            // email, ranked with Reply on top (the Superhuman boost).
            const cmds = [
                { name: 'Reply', desc: 'Reply to sender', shortcut: 'r', action: 'reply' },
                { name: 'Reply All', desc: 'Reply to all', shortcut: 'a', action: 'reply-all' },
                { name: 'Forward', desc: 'Forward email', shortcut: 'f', action: 'forward' },
                { name: 'Archive', desc: 'Archive email', shortcut: 'e', action: 'archive' },
                { name: 'Trash', desc: 'Move to trash', shortcut: '#', action: 'trash' },
                { name: 'Star', desc: 'Toggle star', shortcut: 's', action: 'toggle-flag' },
                { name: 'Mark Unread', desc: 'Toggle unread', shortcut: 'u', action: 'toggle-unread' },
            ];
            // RSVP only when the open email is a calendar invite — mirror the
            // y/n/m keybinding gate (keydown handler ~:3492) so the palette
            // never offers an RSVP that then no-ops.
            if (state.currentEmail?.calendarEvent) {
                cmds.push(
                    { name: 'Accept', desc: 'RSVP accept', shortcut: 'y', action: 'rsvp-accept' },
                    { name: 'Decline', desc: 'RSVP decline', shortcut: 'n', action: 'rsvp-decline' },
                    { name: 'Tentative', desc: 'RSVP tentative', shortcut: 'm', action: 'rsvp-tentative' },
                );
            }
            // Global tail (Superhuman Rule #5: most commands available from
            // everywhere), ranked below the view-native ones.
            cmds.push(
                { name: 'Compose', desc: 'New email', shortcut: 'c', action: 'compose' },
                { name: 'Refresh', desc: 'Reload emails', shortcut: 'R', action: 'refresh' },
                { name: 'Search', desc: 'Search emails', shortcut: '/', action: 'search' },
                { name: 'Go to Inbox', desc: 'Switch to inbox', shortcut: '', action: 'inbox' },
                { name: 'Go to Archive', desc: 'Switch to archive', shortcut: '', action: 'go-archive' },
                { name: 'Go to Trash', desc: 'Switch to trash', shortcut: '', action: 'go-trash' },
                { name: 'New Split', desc: 'Create split inbox', shortcut: '', action: 'new-split' },
                { name: 'Add Account', desc: 'Connect a new mailbox', shortcut: '', action: 'add-account' },
                { name: 'Help', desc: 'Show shortcuts', shortcut: '?', action: 'help' },
            );
            return cmds;
        }
        case 'list': {
            // List's primary actions are the global set; the row actions
            // (archive/trash/star/unread) only apply with a selection —
            // visibleRows()[state.selectedIndex] is the same selection the
            // keydown handler's j/k/o/e/#/s/u act on.
            const cmds = [
                { name: 'Compose', desc: 'New email', shortcut: 'c', action: 'compose' },
                { name: 'Refresh', desc: 'Reload emails', shortcut: 'R', action: 'refresh' },
                { name: 'Search', desc: 'Search emails', shortcut: '/', action: 'search' },
                { name: 'Go to Inbox', desc: 'Switch to inbox', shortcut: '', action: 'inbox' },
                { name: 'Go to Archive', desc: 'Switch to archive', shortcut: '', action: 'go-archive' },
                { name: 'Go to Trash', desc: 'Switch to trash', shortcut: '', action: 'go-trash' },
                { name: 'New Split', desc: 'Create split inbox', shortcut: '', action: 'new-split' },
                { name: 'Add Account', desc: 'Connect a new mailbox', shortcut: '', action: 'add-account' },
            ];
            if (visibleRows()[state.selectedIndex]) {
                cmds.push(
                    { name: 'Archive', desc: 'Archive email', shortcut: 'e', action: 'archive' },
                    { name: 'Trash', desc: 'Move to trash', shortcut: '#', action: 'trash' },
                    { name: 'Star', desc: 'Toggle star', shortcut: 's', action: 'toggle-flag' },
                    { name: 'Mark Unread', desc: 'Toggle unread', shortcut: 'u', action: 'toggle-unread' },
                );
            }
            // Per-split Delete commands: splits show as tabs on the list
            // view, so deleting one is a list-screen action. The old flat
            // getCommands emitted these globally; the context pass dropped
            // them, leaving deleteSplit unreachable (roborev 375, High).
            state.splits.forEach(split => {
                cmds.push({
                    name: `Delete Split: ${split.name}`,
                    desc: `Remove the "${split.name}" split`,
                    shortcut: '',
                    action: `delete-split:${split.id}`,
                });
            });
            cmds.push({ name: 'Help', desc: 'Show shortcuts', shortcut: '?', action: 'help' });
            return cmds;
        }
        case 'compose':
            // Compose is a low-action surface: only the draft-only commands
            // (Superhuman's literal examples — Send / Close Draft / Attach)
            // plus a thin Help tail. These map to the same functions the
            // Ctrl+Enter / Esc / 'a' keybindings already call. Labeled 'Close
            // Draft' (not 'Discard') because escapeCompose keeps the draft
            // saved on the server (kata wm57) — 'Discard' would promise
            // deletion and deliver persistence (roborev 375, Medium).
            return [
                { name: 'Send', desc: 'Send email', shortcut: '\u2318\u23ce', action: 'send' },
                { name: 'Close Draft', desc: 'Keep draft saved and return to list', shortcut: 'Esc', action: 'close-draft' },
                { name: 'Attach', desc: 'Attach a file', shortcut: 'a', action: 'attach' },
                { name: 'Help', desc: 'Show shortcuts', shortcut: '?', action: 'help' },
            ];
        case 'settings': {
            // Settings is a low-action surface: account management + Help only.
            const cmds = [
                { name: 'Add Account', desc: 'Connect a new mailbox', shortcut: '', action: 'add-account' },
            ];
            state.accounts.forEach(acct => {
                const label = acct.email || acct.id;
                cmds.push({
                    name: `Remove Account: ${label}`,
                    desc: `Disconnect and delete cached tokens for ${label}`,
                    shortcut: '',
                    action: `remove-account:${acct.id}`,
                });
            });
            cmds.push({ name: 'Help', desc: 'Show shortcuts', shortcut: '?', action: 'help' });
            return cmds;
        }
        default:
            // Unknown view (defensive): the global set, view-native-agnostic.
            return [
                { name: 'Compose', desc: 'New email', shortcut: 'c', action: 'compose' },
                { name: 'Refresh', desc: 'Reload emails', shortcut: 'R', action: 'refresh' },
                { name: 'Search', desc: 'Search emails', shortcut: '/', action: 'search' },
                { name: 'Go to Inbox', desc: 'Switch to inbox', shortcut: '', action: 'inbox' },
                { name: 'Go to Archive', desc: 'Switch to archive', shortcut: '', action: 'go-archive' },
                { name: 'Go to Trash', desc: 'Switch to trash', shortcut: '', action: 'go-trash' },
                { name: 'New Split', desc: 'Create split inbox', shortcut: '', action: 'new-split' },
                { name: 'Add Account', desc: 'Connect a new mailbox', shortcut: '', action: 'add-account' },
                { name: 'Help', desc: 'Show shortcuts', shortcut: '?', action: 'help' },
            ];
    }
}

function getCommands() {
    return commandsForView(state.view);
}

function executeCommand(action) {
    switch (action) {
        case 'archive': actionSelected('archive'); break;
        case 'trash': actionSelected('trash'); break;
        case 'reply': startReply(false); break;
        case 'reply-all': startReply(true); break;
        case 'compose': startCompose(); break;
        case 'forward': startForward(); break;
        // Compose-screen commands (kata sefy): each calls the SAME function the
        // compose keybinding calls — sendEmail() (Ctrl/Cmd+Enter), the discard
        // path (Esc, here named escapeCompose), and the attachment picker
        // ('a' / Ctrl+Shift+A). No new handlers.
        case 'send': sendEmail(); break;
        case 'close-draft': escapeCompose(); break;
        case 'attach': els.composeFileInput.click(); break;
        // RSVP (kata sefy): the detail palette offers these only behind the
        // calendarEvent gate; rsvpToEvent is the same function y/n/m call.
        case 'rsvp-accept': rsvpToEvent('ACCEPTED'); break;
        case 'rsvp-decline': rsvpToEvent('DECLINED'); break;
        case 'rsvp-tentative': rsvpToEvent('TENTATIVE'); break;
        case 'search': openSearch(); break;
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
        // state.currentAccount is an object, not an id string — compare its
        // .id, else this reset branch is dead and deleting the in-use account
        // leaves dangling state (kata hp8w).
        if (state.currentAccount?.id === id) {
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

    if (!state.currentAccount?.id) {
        showStatus('Select an account before creating a split', 'error');
        return;
    }

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
            // New splits belong to the account being viewed; hand-edit
            // splits.json to make one global.
            account: state.currentAccount?.id,
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

    // Optimistic: re-insert the email into the list immediately. insertIndex is
    // a state.emails (DATA) position — correct for restoring sort order — but
    // selection must land on the re-inserted email's VISIBLE row, which under
    // grouping is not that flat index (kata 64z6).
    if (item.emailData) {
        const idx = Math.min(item.insertIndex, state.emails.length);
        state.emails.splice(idx, 0, item.emailData);
        // Guarantee the id is registered even in the edge case where its thread
        // was never grouped; extend is idempotent per id.
        extendThreadGroups([item.emailData]);
        state.selectedIndex = visibleRowIndexForEmailId(item.emailId);
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

// Self-hosted dashboard-icons assets. A non-empty img alt is both the
// accessible provider name and the browser's text fallback if an asset cannot
// be rendered. Unknown future providers fall back to escaped visible text.
const PROVIDER_ICONS = new Map([
    ['gmail', { label: 'Gmail', src: '/provider-icons/gmail.svg' }],
    ['outlook', { label: 'Outlook', src: '/provider-icons/microsoft-outlook.svg' }],
    ['fastmail', { label: 'Fastmail', src: '/provider-icons/fastmail.svg' }],
]);

function providerIcon(provider) {
    const label = typeof provider === 'string' && provider ? provider : 'Unknown provider';
    const icon = PROVIDER_ICONS.get(label.toLowerCase());
    if (!icon) return `<span class="provider-icon-fallback">${escapeHtml(label)}</span>`;
    const escapedLabel = escapeAttr(icon.label);
    return `<img class="provider-icon" src="${icon.src}" width="16" height="16" alt="${escapedLabel}" title="${escapedLabel}">`;
}

// escapeHtml is safe for text content but textContent's serializer doesn't
// encode `"` or `'`, so a value with quotes can break out of an attribute.
// Use escapeAttr inside attribute strings like data-foo="${...}".
function escapeAttr(text) {
    return escapeHtml(text).replace(/"/g, '&quot;').replace(/'/g, '&#39;');
}

// Render attacker-controlled email HTML in a sandboxed iframe. The sandbox
// token list deliberately omits allow-scripts, so scripts inside the iframe
// do not run at all — closing the entire class of HTML-sanitizer bypasses
// (mXSS, scheme tricks, namespace confusion, future parser quirks).
// allow-same-origin is granted (still safe with allow-scripts absent: no JS
// runs in the iframe, so the token only lets the parent measure/scroll passive
// DOM) so the parent can size the iframe to its content and drive it with the
// same keyboard shortcuts as plain-text email. allow-popups +
// allow-popups-to-escape-sandbox lets links click through to new tabs as a
// normal browsing context; <base target=_blank> in the srcdoc makes all links
// default to opening externally.
// Header is trusted HTML (caller composed it from escapeHtml output); body is
// attacker-controlled and goes into an iframe via renderHtmlBodyIframe.
function renderComposeQuote(headerHtml, quotedHtml, quotedText) {
    const headerEl = document.createElement('p');
    headerEl.innerHTML = headerHtml;
    els.composeQuote.replaceChildren(headerEl);
    if (quotedHtml) {
        const bodyHost = document.createElement('div');
        bodyHost.className = 'compose-quote-body';
        els.composeQuote.appendChild(bodyHost);
        renderHtmlBodyIframe(bodyHost, quotedHtml, { autosize: true });
    } else {
        const pre = document.createElement('pre');
        pre.textContent = quotedText;
        els.composeQuote.appendChild(pre);
    }
    els.composeQuote.classList.remove('hidden');
}

// Both sides use sandbox="allow-same-origin" with NO allow-scripts: scripts in
// the iframe never run, closing the whole class of HTML-sanitizer bypasses.
// Same-origin is safe *precisely because* allow-scripts is absent — no JS runs
// in the iframe, so the token only lets the parent measure/scroll passive DOM.
//
// Read-side (default): additionally keeps allow-popups +
// allow-popups-to-escape-sandbox so recipient links click through to new tabs
// (<base target=_blank> makes that the default), AND sizes the iframe to its
// full content height so the iframe never scrolls internally — the PARENT pane
// (#email-body) scrolls instead. That is what makes Space / Shift-Space and
// scroll-position memory work for HTML emails the same as for plain text: the
// document-level keydown handler scrolls els.emailBody, not a cross-origin
// iframe it can't reach.
//
// Compose-quote side (opts.autosize=true): sizes to fit too, but lives in a
// non-scrolling host, so there is no scroll position to restore.
function renderHtmlBodyIframe(container, html, opts) {
    const autosize = !!(opts && opts.autosize);
    const scrollTop = (opts && opts.scrollTop) || 0;
    // Disconnect a prior iframe's ResizeObserver so it can't pin the previous
    // email's DOM in memory across navigations.
    const oldIframe = container.querySelector('iframe.email-iframe');
    if (oldIframe && oldIframe._ro) oldIframe._ro.disconnect();
    // Abort the prior iframe's image load/error listeners so never-firing
    // images can't pin the previous email's DOM in memory across navigations
    // (mirrors the _ro.disconnect() above; kata ceph remaining gap).
    if (oldIframe && oldIframe._imgLoadAc) oldIframe._imgLoadAc.abort();
    container.replaceChildren();
    const iframe = document.createElement('iframe');
    iframe.setAttribute(
        'sandbox',
        autosize ? 'allow-same-origin' : 'allow-same-origin allow-popups allow-popups-to-escape-sandbox'
    );
    iframe.className = 'email-iframe';
    iframe.setAttribute('srcdoc', wrapEmailHtml(linkifyHtml(html)));
    iframe.addEventListener('load', () => {
        sizeIframeToContent(iframe);
        // Restore the saved scroll position now that the iframe is sized and
        // the parent pane is scrollable. Compose-quote (autosize) sits in a
        // non-scrolling host, so there is nothing to restore.
        if (!autosize) container.scrollTop = scrollTop;
    });
    container.appendChild(iframe);
}

// Size a sandboxed (allow-same-origin, no-scripts) iframe to its content's
// full height so it never scrolls internally — the parent pane scrolls
// instead, which is what keeps the document-level keyboard shortcuts working.
//
// The iframe `load` event fires when the DOM is PARSED, not when LAYOUT is
// complete: images without explicit dimensions contribute height 0 until
// they load, web fonts reflow text after load, large tables lay out late.
// A single load-time `body.scrollHeight` therefore returns a PARTIAL height,
// and setting the iframe to that clips the email below it — the "only the top
// 10%" bug (ceph). Measure on several cues:
//
//   - load + rAF + a short timeout: catch post-parse layout on the next
//     frame and a tick later (covers layout that rAF misses). Only-GROW on
//     these burst cues — a stale smaller read here must not clip an already-
//     larger iframe (the original bug). Settled cues (below) may shrink.
//   - document.fonts.ready + ResizeObserver: "settled" cues that fire AFTER
//     the burst. These may also SHRINK the iframe, because content can
//     legitimately get smaller after settling — a web font swapping in
//     SHORTER than the fallback, or an image erroring out after reserving
//     height. Without shrink here, Space pages past the end of the email
//     through trailing blank space.
//
// Math.max(body.scrollHeight, documentElement.scrollHeight) is the true full
// content height for the GROW path — body.scrollHeight alone can under-report
// under collapsed-margins / sender-authored inline body styles. For the
// SETTLED path (which may shrink) documentElement.scrollHeight is the WRONG
// measure: in standards mode (the srcdoc has <!doctype html>) it's floored at
// the viewport height, and the iframe's viewport IS its current style.height,
// so h >= cur always and shrink never fires. Use
// documentElement.getBoundingClientRect().height there instead — the root
// element's height is `auto`, so its border box tracks CONTENT, not the
// viewport, and a real content shrink (shorter web font, image error) is
// reflected (roborev on ceph).
//
// Ratchet guard: viewport-relative sender CSS (min-height:100vh, common in
// marketing emails) makes documentElement.scrollHeight = viewport + wrapper
// padding, so every grow triggers another grow of ~+padding — ~4k relayouts
// to hit the cap. An absolute ceiling bounds the worst case, and a
// no-progress guard stops the ratchet early: once a grow's delta falls below
// a small epsilon (just the wrapper padding, not real content) we stop
// growing from observer-driven cues (roborev on ceph).
const EMAIL_IFRAME_MAX_HEIGHT = 200_000;
const EMAIL_IFRAME_RATCHET_EPSILON = 64; // ~wrapper padding; smaller grows are noise

function sizeIframeToContent(iframe) {
    // `canShrink` distinguishes the load burst (only-grow — a stale small read
    // must not clip) from settled cues (fonts.ready, ResizeObserver) that fire
    // AFTER layout settles and may legitimately shrink (web font shorter than
    // fallback, image error after reserving height — otherwise Space pages
    // past the end through trailing blank space).
    // Three modes share one measure:
    //   'burst'    — load/rAF/timeout. Grow-only, NO epsilon guard: a genuine
    //     small correction (< 64px, a line or three the first measure missed)
    //     must apply or the email stays permanently clipped (the ceph bug).
    //   'oneshot'  — fonts.ready. Shrink-capable, grow without epsilon: a
    //     one-shot cue cannot ratchet, so a small font-swap grow must apply.
    //   'observer' — ResizeObserver. Shrink-capable, grow with a CONSECUTIVE-
    //     small-grow suppression: the viewport-relative ratchet (min-height:
    //     100vh sender CSS) self-sustains ONLY on this recurring path, growing
    //     ~48px per tick to the cap. Suppress after N consecutive sub-epsilon
    //     grows; any real grow (>= epsilon) or any shrink resets the count.
    const measure = (mode) => () => {
        try {
            const doc = iframe.contentDocument;
            if (!doc) return;
            const body = doc.body;
            if (!body) return;
            // Measure the true content height. The grow path uses scrollHeight
            // (the established full-height measure). The shrink-capable paths
            // use getBoundingClientRect().height on the root AND body.
            // scrollHeight — but NOT documentElement.scrollHeight: in standards
            // mode the root's scrollHeight is floored at the viewport (=
            // current style.height), so including it makes h >= cur always and
            // shrink can never fire. The rect (root height:auto) tracks
            // content, not the viewport; body.scrollHeight tracks content too
            // and covers a pinned root (sender CSS html{height:100%}, common in
            // email templates) where the rect is clamped to the viewport but
            // body still grows when a late image loads. Their max is the true
            // content height for the settled paths (roborev ceph).
            const canShrink = mode !== 'burst';
            let h;
            if (canShrink) {
                const rect = doc.documentElement.getBoundingClientRect();
                h = Math.max(rect.height, body.scrollHeight);
            } else {
                // Grow path uses scrollHeight (the established full-height
                // measure). Computed here, not above, so the settled (recurring
                // observer) path doesn't pay for a forced documentElement
                // reflow on every tick (roborev on ceph).
                const rootScrollH = doc.documentElement.scrollHeight;
                h = Math.max(body.scrollHeight, rootScrollH);
            }
            // Ratchet guard: cap at an absolute ceiling so viewport-relative
            // sender CSS (min-height:100vh) can't grow the iframe unboundedly.
            h = Math.min(h, EMAIL_IFRAME_MAX_HEIGHT);
            if (h <= 0) return;
            const cur = parseFloat(iframe.style.height) || 0;
            if (canShrink) {
                // Epsilon equality so sub-pixel fractional rect heights
                // don't thrash the style on every observer tick. A near-equal
                // read also clears any pending shrink: content recovered to
                // ~cur before the confirm fired, so the pending smaller value
                // was transient and must not be re-armed by a later dip
                // (roborev on ceph).
                if (Math.abs(h - cur) < 1) {
                    iframe._pendingShrink = undefined;
                    return;
                }
                if (h < cur) {
                    // Shrink is the RISKY direction: a self-sustaining
                    // DOWNWARD ratchet is possible on viewport-proportional
                    // sender CSS (a 90vh hero: shrink the iframe → shrink the
                    // viewport → shrink the vh-sized body → shrink again,
                    // collapsing toward the in-flow floor). The in-flow floor
                    // is built into the measurement (h = max of rect.height and
                    // body.scrollHeight), so a shrink can't clip in-flow
                    // content — but vh-dominant email CAN collapse to near-zero
                    // in-flow content. TWO defenses:
                    //   - Delayed self-confirmation: don't shrink on the first
                    //     smaller read; record it and re-measure after 200ms. A
                    //     real content shrink (shorter web font, image error) is
                    //     stable and confirms; a one-off transient dip recovers
                    //     and clears the pending value.
                    //   - Consecutive-shrink suppression (mirrors
                    //     _smallGrowStreak): each ratchet STEP is stable until
                    //     the shrink write, so the confirm alone can't stop the
                    //     loop — the iframe still collapses geometrically in
                    //     200ms steps. After N confirmed shrinks with no
                    //     intervening >=epsilon grow, stop shrinking from the
                    //     observer path; a real grow resets the streak. This is
                    //     what actually breaks the ratchet (roborev ceph).
                    const prev = iframe._pendingShrink;
                    if (prev === undefined || Math.abs(prev - h) >= 1) {
                        // First smaller read, or a different value than the
                        // pending one — record and schedule a confirmation.
                        // Single-pending guard (mirrors _pendingGrowConfirm):
                        // a re-arm during the 200ms window must update the
                        // pending VALUE but not stack a second timer, or an
                        // earlier timer confirms a newer value after <200ms
                        // of stability (ceph shrink parity).
                        iframe._pendingShrink = h;
                        if (!iframe._pendingShrinkConfirm) {
                            iframe._pendingShrinkConfirm = true;
                            setTimeout(() => {
                                // Re-measure via the observer path; if the
                                // value is still ~h, it's stable and the
                                // shrink applies. If layout changed again,
                                // _pendingShrink was updated by that read and
                                // this confirm is a no-op.
                                iframe._pendingShrinkConfirm = false;
                                observerFn();
                            }, 200);
                        }
                        return;
                    }
                    // Stable confirmed shrink (this read ~= the pending one).
                    // Consecutive-shrink suppression: stop after a short streak
                    // of confirmed shrinks with no intervening grow — the
                    // viewport-proportional ratchet, not a real content shrink.
                    // Quiet-gap reset (mirrors the grow side): the ratchet runs
                    // at a tight ~200ms cadence, so well-separated legit
                    // shrinks (blocked images releasing height minutes apart)
                    // reset the streak and still reclaim blank space (roborev ceph).
                    const nowS = Date.now();
                    const lastShrink = iframe._lastShrinkAt || 0;
                    if (nowS - lastShrink > 1000) iframe._shrinkStreak = 0;
                    iframe._lastShrinkAt = nowS;
                    iframe._shrinkStreak = (iframe._shrinkStreak || 0) + 1;
                    if (iframe._shrinkStreak > 2) {
                        iframe._pendingShrink = undefined;
                        // Escape hatch (mirrors the suppressed-grow confirm,
                        // but bounded per episode): three LEGIT shrinks within
                        // ~1s (several erroring images) also trip the streak,
                        // and with no further observer ticks the trailing
                        // blank space would persist forever. Schedule a
                        // single-pending re-measure past the 1s quiet-gap
                        // reset so that case reclaims its space. UNLIKE the
                        // grow side, a suppressed ratchet here is QUIESCENT
                        // (once shrink writes stop, the vh-sized body's
                        // border-box stops changing, so no observer tick can
                        // fire first) — an unbounded re-arm would reset the
                        // streak every 1200ms and restart the collapse cycle
                        // indefinitely. The legit case needs exactly ONE
                        // re-measure, so cap the hatch at one use per episode;
                        // only a real grow (content actually changed) resets
                        // the counter (roborev 389).
                        if (!iframe._pendingShrinkConfirm
                            && !(iframe._shrinkEscapeCount >= 1)) {
                            iframe._shrinkEscapeCount =
                                (iframe._shrinkEscapeCount || 0) + 1;
                            iframe._pendingShrinkConfirm = true;
                            setTimeout(() => {
                                iframe._pendingShrinkConfirm = false;
                                observerFn();
                            }, 1200);
                        }
                        return; // ratchet — stop collapsing
                    }
                    iframe._pendingShrink = undefined;
                    iframe._smallGrowStreak = 0;
                    iframe.style.height = h + 'px';
                } else {
                    // Grow on a settled path — clears any pending shrink (the
                    // content grew, the earlier smaller read was transient) and
                    // resets the shrink streak AND the escape-hatch counter (a
                    // real grow ends the shrink episode; the next legit shrink
                    // run gets its one re-measure again).
                    iframe._pendingShrink = undefined;
                    iframe._shrinkStreak = 0;
                    iframe._shrinkEscapeCount = 0;
                    if (mode === 'observer'
                        && h - cur < EMAIL_IFRAME_RATCHET_EPSILON) {
                        // Sub-epsilon grow on the RECURRING path: could be the
                        // viewport-relative ratchet OR several real small
                        // images loading sequentially on a slow connection
                        // (icons/badges < 64px each). Suppressing the 3rd+
                        // would clip the tail of the image sequence — WORSE
                        // than the ratchet, which is already hard-bounded by
                        // EMAIL_IFRAME_MAX_HEIGHT. So: only count it toward the
                        // streak if the ticks are RAPID (the ratchet self-
                        // sustains at ~one observer callback per layout, back-
                        // to-back); a quiet gap (>500ms with no observer tick)
                        // means a new image loaded, not a ratchet — reset the
                        // streak and let the grow apply (roborev on ceph).
                        const now = Date.now();
                        const lastTick = iframe._observerLastTick || 0;
                        iframe._observerLastTick = now;
                        if (now - lastTick > 500) {
                            iframe._smallGrowStreak = 0; // quiet gap — new image, not ratchet
                        }
                        iframe._smallGrowStreak = (iframe._smallGrowStreak || 0) + 1;
                        if (iframe._smallGrowStreak > 2) {
                            // Suppressed — but a real small grow (several small
                            // images on a FAST connection, <500ms apart) must
                            // still eventually apply or the email stays clipped
                            // by up to ~63px (a small version of the ceph bug).
                            // Schedule a one-shot delayed re-measure AFTER the
                            // quiet-gap threshold: the self-scheduled confirm
                            // re-enters this branch, and with now-lastTick > 500
                            // it RESETS the streak and applies the grow. The
                            // ratchet self-sustains at one tick per layout, so
                            // it fires again before the 600ms confirm and re-
                            // suppresses — bounded to ~epsilon per 600ms period
                            // (no perpetual loop: a single pending confirm per
                            // iframe — only the timer callback clears the flag;
                            // an intervening real observer tick just fires a
                            // benign redundant re-measure). A real grow with no
                            // further ratchet ticks confirms and applies
                            // (roborev on ceph).
                            if (!iframe._pendingGrowConfirm) {
                                iframe._pendingGrowConfirm = true;
                                setTimeout(() => {
                                    iframe._pendingGrowConfirm = false;
                                    observerFn();
                                }, 600);
                            }
                            return;
                        }
                    } else {
                        iframe._smallGrowStreak = 0;
                    }
                    iframe.style.height = h + 'px';
                }
            } else if (h > cur) {
                // Burst: only-grow, no epsilon guard (a genuine small
                // correction must apply — see mode doc above). A burst grow is
                // a REAL content grow (image load / rAF / load — the downward
                // ratchet never produces one), so it also ends the shrink
                // episode: under pinned-body sender CSS the observer never
                // fires and image events are the only cues, so without this
                // reset a second legit erroring-image burst would get no
                // escape-hatch re-measure and its blank space would persist
                // for the life of the view (roborev 390).
                iframe._shrinkStreak = 0;
                iframe._shrinkEscapeCount = 0;
                iframe.style.height = h + 'px';
            }
        } catch (_) { /* allow-same-origin should always succeed */ }
    };
    const grow = measure('burst');
    const oneshot = measure('oneshot');
    const observerFn = measure('observer');
    // Immediate + next layout tick + a short delay for post-load layout rAF
    // misses. Burst cues: only-grow, no epsilon guard.
    grow();
    requestAnimationFrame(grow);
    setTimeout(grow, 0);
    // Settled one-shot cue: web fonts reflow text after load. Shrink-capable
    // (font shorter than fallback), grow without epsilon (one-shot, can't
    // ratchet) so a small font-swap grow applies.
    try {
        const fonts = iframe.contentDocument?.fonts;
        if (fonts?.ready) fonts.ready.then(oneshot);
    } catch (_) { /* allow-same-origin should always succeed */ }
    // Ongoing re-measure for images loading after open. Recurring settled cue:
    // shrink-capable, grow with consecutive-small suppression (the ratchet
    // defense). Assign _ro BEFORE observe() so a thrown observer is still
    // disconnectable; attach is effectively one-shot from the load handler, so
    // if body is null here ongoing tracking is lost for this open (rare — body
    // exists at load in practice) (roborev on ceph).
    if (!iframe._sized) {
        try {
            const body = iframe.contentDocument && iframe.contentDocument.body;
            if (!body) return;
            const ro = new ResizeObserver(observerFn);
            iframe._ro = ro;
            ro.observe(body);
            iframe._sized = true;
        } catch (_) { /* allow-same-origin should always succeed */ }
    }
    // ceph remaining gap: a late-loading image (no explicit dimensions) grows
    // body.scrollHeight but, under height-pinned sender CSS (html,body{height:
    // 100%} — common in email templates), does NOT change body's border-box, so
    // the ResizeObserver above never fires and the iframe stays clipped at the
    // partial "top 10%" height the load burst measured. Re-measure on each
    // image's load (grow-only burst: a late image must fit) and error (settled
    // observer path: a broken image reclaiming reserved height may shrink)
    // event — these fire independent of the body border-box the RO watches, so
    // they catch the pinned-body case the RO cannot. An AbortController removes
    // every listener at once on re-render (renderHtmlBodyIframe aborts
    // _imgLoadAc) so never-firing images can't pin the previous email's DOM.
    try {
        const idoc = iframe.contentDocument;
        if (idoc && idoc.querySelectorAll) {
            const imgs = idoc.querySelectorAll('img');
            if (imgs && imgs.length) {
                const ac = new AbortController();
                iframe._imgLoadAc = ac;
                imgs.forEach((img) => {
                    img.addEventListener('load', grow, { signal: ac.signal });
                    img.addEventListener('error', observerFn, { signal: ac.signal });
                });
            }
        }
    } catch (_) { /* allow-same-origin should always succeed */ }
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
    return doc.body.innerHTML;
}

// HTML emails are authored against a white canvas: senders routinely set
// explicit dark text colors and no background. The iframe therefore always
// gets a light canvas, independent of the app theme — a dark surface under
// sender-colored text is what made messages unreadable (kata tgax). Emails
// that set their own background override these html,body defaults (nothing
// here uses !important on colors). Plain-text bodies never come through
// here and keep following the app theme.
function wrapEmailHtml(html) {
    return '<!doctype html><html><head>'
        + '<meta charset="utf-8">'
        + '<base target="_blank">'
        + '<meta name="color-scheme" content="light">'
        + '<style>'
        + 'html,body{margin:0;padding:12px;background:#fff;color:#222;'
        + 'font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;'
        + 'font-size:14px;line-height:1.5;word-wrap:break-word;overflow-wrap:break-word;}'
        + 'img{max-width:100%;height:auto;}'
        + 'a{color:#0366d6;}'
        + 'blockquote,.gmail_quote{border-left:2px solid #ccc;padding-left:12px;margin-left:0;color:#555;}'
        + 'table{border-collapse:collapse;}'
        + 'td,th{padding:4px 8px;}'
        + 'pre,code{background:#f4f4f4;padding:2px 4px;border-radius:3px;}'
        + '*{writing-mode: horizontal-tb !important;text-orientation: mixed !important;}'
        + '</style>'
        + '</head><body>'
        + html
        + '</body></html>';
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

function renderAttachments(attachments, emailId, account) {
    els.attachments.classList.remove('hidden');
    const downloadAllBtn = attachments.length > 1
        ? `<a class="attachments-download-all" onclick="downloadAllAttachments(event)">Download All</a>`
        : '';
    const header = `<div class="attachments-header"><span>📎 Attachments (${attachments.length})</span>${downloadAllBtn}</div>`;
    // Plain hrefs can't go through api()'s auto ?account= appending, so pin
    // the account here — the email's own, not the globally current one:
    // without it the server resolves blob ids against the default account.
    const acct = account || state.currentAccount?.id;
    const acctQuery = acct ? `?account=${encodeURIComponent(acct)}` : '';
    const items = attachments.map(att => {
        const icon = getFileIcon(att.mime_type, att.name);
        const size = formatFileSize(att.size);
        const url = `/api/emails/${emailId}/attachments/${encodeURIComponent(att.blob_id)}/${encodeURIComponent(att.name)}${acctQuery}`;
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
    const showActions = event.method === 'REQUEST';
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

    // Show/hide updated banner (rescheduled invite — non-destructive; distinct
    // from the cancelled banner). user_rsvp_status is None on an update, so the
    // RSVP buttons already render un-highlighted below.
    const isUpdate = !!event.isUpdate && !cancelled;
    let updBanner = els.calendarEvent.querySelector('.cal-updated');
    if (isUpdate) {
        if (!updBanner) {
            updBanner = document.createElement('div');
            updBanner.className = 'cal-updated';
            updBanner.textContent = 'Updated — please respond again';
            card.querySelector('.cal-header').after(updBanner);
        }
    } else if (updBanner) {
        updBanner.remove();
    }

    // Render attendees
    if (event.attendees && event.attendees.length > 0) {
        const attendeeList = event.attendees.map(a => {
            const name = a.name || a.email;
            const statusIcon = getStatusIcon(a.status);
            // statusIcon is trusted-by-construction (getStatusIcon returns one
            // of four fixed <span> constants we control) — only a.email and
            // name are attacker-controlled. a.email sits in an attribute, so it
            // needs escapeAttr (escapeHtml doesn't encode quotes, and a crafted
            // email like " onmouseover="alert(1) breaks out of title="…" at
            // parse time — no hover required); name is text content, so
            // escapeHtml suffices (kata yane).
            return `<span class="attendee" title="${escapeAttr(a.email)}">${statusIcon} ${escapeHtml(name)}</span>`;
        }).join(', ');
        els.calAttendees.innerHTML = `<span class="label">Attendees:</span> ${attendeeList}`;
        els.calAttendees.style.display = 'block';
    } else {
        els.calAttendees.style.display = 'none';
    }

    // Find current user's RSVP status. On an Update (rescheduled invite) the
    // response was reset server-side — force null so the attendee-scan fallback
    // can't resurrect a stale highlight from the incoming ICS's PARTSTAT.
    const userStatus = event.isUpdate ? null : (event.user_rsvp_status || getUserRsvpStatus(event));

    // Show RSVP actions only for invitations that request a response
    const actions = els.calendarEvent.querySelector('.calendar-actions');
    if (!showActions) {
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
    const listItem = state.emails.find(e => e.id === state.currentEmail.id);
    const prevInviteStatus = listItem?.inviteStatus;
    const prevInviteIsUpdated = listItem?.inviteIsUpdated;

    // Optimistic: update RSVP buttons and the hidden list row immediately,
    // so returning to the inbox shows the new chip without a list reload.
    if (listItem?.isInviteToMe) {
        listItem.inviteStatus = status;
        listItem.inviteIsUpdated = false;
        renderEmailList();
    }
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
            emailCache[cacheKey(state.currentEmail.id)] = state.currentEmail;
            renderCalendarCard(result.calendarEvent);
        }
    } catch (err) {
        // Revert optimistic update if we had one
        if (prevEvent) {
            state.currentEmail.calendarEvent = prevEvent;
            emailCache[cacheKey(state.currentEmail.id)] = state.currentEmail;
            renderCalendarCard(prevEvent);
        }
        if (listItem?.isInviteToMe) {
            listItem.inviteStatus = prevInviteStatus;
            listItem.inviteIsUpdated = prevInviteIsUpdated;
            renderEmailList();
        }
        showStatus('Failed to send RSVP: ' + err.message, 'error');
    }
}

// Initialize on load
document.addEventListener('DOMContentLoaded', init);

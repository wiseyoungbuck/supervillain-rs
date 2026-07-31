// Shared e2e fixtures: the /api/* mock responses the app boots against.
//
// Every spec uses page.route() to intercept /api/* and return this deterministic
// data, so no real provider is ever hit. The shapes match what the real server
// emits (src/routes.rs list_accounts/list_mailboxes/list_emails), minimal —
// only the fields the app reads at boot and in the ticket-under-test.

// One connected Fastmail-ish account. authStatus !== 'pending' so the app
// auto-selects it (loadAccounts → selectAccount → loadMailboxes).
const ACCOUNTS = {
  accounts: [
    {
      id: 'acct-1',
      email: 'me@example.com',
      provider: 'fastmail',
      authStatus: 'connected',
      isDefault: true,
    },
  ],
  errors: [],
};

// Mailboxes including one with an XSS-payload name (the 1p0d repro) and one
// with an XSS-payload id (the attribute-breakout variant). role:'inbox' makes
// the inbox first in the sort; the payload mailbox has no role so it sorts
// after, parentId:null so renderMailboxes includes it.
const MAILBOXES_WITH_XSS = [
  { id: 'm-inbox', name: 'Inbox', role: 'inbox', parentId: null, unreadEmails: 2 },
  // 1p0d: a delegated/shared mailbox whose name contains HTML. Before the fix
  // this rendered as a live <img> in the sidebar; after, it must render as text.
  {
    id: 'm-shared',
    name: '<img src=x onerror="window.__xss_mailbox=1">',
    role: null,
    parentId: null,
    unreadEmails: 0,
  },
  // 1p0d attribute variant: an id that would break out of data-id="..." if
  // unescaped (escapeHtml doesn't encode quotes; escapeAttr does).
  {
    id: 'm-evil" onmouseover="window.__xss_attr=1',
    name: 'Evil Id Mailbox',
    role: null,
    parentId: null,
    unreadEmails: 0,
  },
];

// A plain inbox with one email, used as the default list.
const ONE_EMAIL_LIST = [
  {
    id: 'e-1',
    subject: 'Hello',
    from: [{ name: 'Sender', email: 'sender@example.com' }],
    to: [{ name: 'Me', email: 'me@example.com' }],
    receivedAt: new Date().toISOString(),
    isUnread: false,
    preview: 'body preview',
    account: 'acct-1',
  },
];

// A calendar invite email (the calendarEvent field is what renderCalendarCard
// consumes). method drives the nt9e gate: 'REQUEST' shows RSVP buttons, anything
// else hides them. Reuse for both the REQUEST and REPLY cases by overriding
// calendarEvent.method per spec.
//
// Variants (kata yq92):
//   isUpdate:   a rescheduled invite (incremented SEQUENCE). Mirrors the
//               server: user_rsvp_status is reset to null and the list row
//               carries inviteIsUpdated so the chip shows 'Updated'.
//   userStatus: the user's existing RSVP (e.g. 'ACCEPTED' for an
//               already-answered invite). Sets both user_rsvp_status and the
//               'me' attendee's PARTSTAT, plus the list row's inviteStatus.
//
// The list-row invite fields (isInviteToMe/inviteMethod/inviteStatus/
// inviteIsUpdated) mirror what list_emails emits (src/routes.rs) so the trbx
// inbox chip renders from the same fixture.
function inviteEmail({ method, summary = 'Sync', emailId = 'e-cal', isUpdate = false, userStatus = null }) {
  const status = userStatus || 'NEEDS-ACTION';
  return {
    id: emailId,
    subject: 'Invite: ' + summary,
    from: [{ name: 'Organizer', email: 'org@example.com' }],
    to: [{ name: 'Me', email: 'me@example.com' }],
    receivedAt: new Date().toISOString(),
    isUnread: false,
    preview: 'calendar invite',
    account: 'acct-1',
    // Inbox-row chip fields (trbx). renderInviteChip gates on
    // isInviteToMe && inviteMethod === 'REQUEST', matching the server's
    // "an invite addressed to me" computation.
    isInviteToMe: method === 'REQUEST',
    inviteMethod: method,
    inviteStatus: method === 'REQUEST' ? status : null,
    inviteIsUpdated: isUpdate,
    calendarEvent: {
      method, // 'REQUEST' | 'REPLY' | 'CANCEL' | 'PUBLISH' | ...
      summary,
      dtstart: '2026-08-01T15:00:00Z',
      dtend: '2026-08-01T16:00:00Z',
      location: 'Zoom',
      isUpdate,
      attendees: [
        { email: 'me@example.com', name: 'Me', status: isUpdate ? 'NEEDS-ACTION' : status },
        { email: 'org@example.com', name: 'Organizer', status: 'ACCEPTED' },
      ],
      // The server resets the response on an update (the user must answer
      // again); otherwise a REQUEST carries the user's current PARTSTAT.
      user_rsvp_status: isUpdate ? null : (method === 'REQUEST' ? status : null),
    },
  };
}

// Intercept the boot + list routes with the given mailboxes and email list.
// `extra` lets a spec add per-route overrides (e.g. a specific email by id).
async function mockApi(page, {
  mailboxes = MAILBOXES_WITH_XSS,
  emails = ONE_EMAIL_LIST,
  splits = [],
  extra = {},
} = {}) {
  await page.route('**/api/accounts', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(ACCOUNTS) })
  );
  await page.route('**/api/mailboxes**', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(mailboxes) })
  );
  await page.route('**/api/emails?**', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(emails) })
  );
  // Default a single-email GET to the first email in the list (or the override).
  const byId = extra['emailById'] || {};
  await page.route('**/api/emails/*', (route) => {
    const id = route.request().url().split('/api/emails/')[1].split('?')[0];
    const email = byId[id] || emails.find((e) => e.id === id) || emails[0];
    return route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(email) });
  });
  // identities, splits, split-counts, theme, timezone: empty/defaults
  // so boot doesn't 404 (the app tolerates empty; we just need no errors thrown).
  // /api/build-id is deliberately NOT mocked: the real server serves it with no
  // provider dependency, and it must match the <meta name="build-id"> the same
  // binary stamped into the shell. Mocking it to a literal made the ids
  // mismatch, so every spec booted with the deploy banner showing — an
  // unintended UI state that killed the deploy poll and shifted layout.
  await page.route('**/api/identities**', (route) => route.fulfill({ status: 200, contentType: 'application/json', body: '[]' }));
  await page.route('**/api/splits**', (route) => route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(splits) }));
  await page.route('**/api/split-counts**', (route) => route.fulfill({ status: 200, contentType: 'application/json', body: '{}' }));
  await page.route('**/api/theme', (route) => route.fulfill({ status: 200, contentType: 'text/css', body: '' }));
  await page.route('**/api/timezone**', (route) => route.fulfill({ status: 200, contentType: 'application/json', body: '{}' }));
  await page.route('**/api/timezone/zones', (route) => route.fulfill({ status: 200, contentType: 'application/json', body: '[]' }));
  // Mutation routes the app fires as side-effects of opening/acting on emails.
  // Mock them as 204/no-content so they don't 404 and surface error toasts that
  // would pollute the status line the specs assert against.
  await page.route('**/api/emails/*/mark-read', (route) => route.fulfill({ status: 204 }));
  await page.route('**/api/emails/*/mark-unread', (route) => route.fulfill({ status: 204 }));
  await page.route('**/api/emails/*/archive', (route) => route.fulfill({ status: 204 }));
  await page.route('**/api/emails/*/trash', (route) => route.fulfill({ status: 204 }));
  await page.route('**/api/emails/*/toggle-flag', (route) => route.fulfill({ status: 204 }));
  // Per-spec extra routes (e.g. the unsubscribe POST).
  for (const [pattern, handler] of Object.entries(extra['routes'] || {})) {
    await page.route(pattern, handler);
  }
}

module.exports = { ACCOUNTS, MAILBOXES_WITH_XSS, ONE_EMAIL_LIST, inviteEmail, mockApi };

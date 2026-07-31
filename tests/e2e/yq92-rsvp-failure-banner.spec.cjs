// E2E spec for kata yq92 (gap 2): honest-failure surfacing for RSVP.
//
// m5yp (CalendarAuthUnconfigured, 400) and wybm (CalendarDiscoveryFailed, 503)
// made the server return an ACTIONABLE error instead of swallowing the CalDAV
// failure. This spec pins the browser half: when POST /api/emails/{id}/rsvp
// fails with one of those errors, the account-error banner renders the
// actionable message (the same persistent surface the server-side
// fire-and-forget auto-add path pushes to — src/routes.rs
// surface_caldav_spawn_failure), the toast shows the human message (not the
// raw {"error": ...} JSON body), and the optimistic card update is reverted.
// A successful RSVP must NOT raise the banner.
//
// The error bodies replicate src/error.rs IntoResponse exactly:
// {"error": <constant>, "code": <machine-readable code>} with status
// 400 / 503 — the client banner-routes on the code, and the error.rs
// contract tests pin that the real server emits it.

const { test, expect } = require('@playwright/test');
const { mockApi, inviteEmail } = require('./fixtures.cjs');

// Mirrors CALENDAR_AUTH_UNCONFIGURED_MSG / CALENDAR_DISCOVERY_FAILED_MSG in
// src/error.rs — the actionable client-facing constants the 400/503 bodies
// carry. Kept verbatim so the spec fails if the server rewords them without
// the UI keeping up.
const AUTH_MSG = 'Fastmail calendar sync needs an app password — add one in Settings';
const DISCOVERY_MSG =
  "Couldn't find your Fastmail calendar to sync the event — check your calendars in Fastmail Settings";

// Boot with one REQUEST invite and an rsvp route that fails with the given
// status + server-shaped error body. accountErrors seeds the /api/accounts
// errors array (other accounts' boot-time banner lines).
async function openInviteWithFailingRsvp(page, { status, error, code = null, accountErrors = [] }) {
  const email = inviteEmail({ method: 'REQUEST', summary: 'Sync' });
  await mockApi(page, {
    emails: [email],
    extra: {
      emailById: { [email.id]: email },
      routes: {
        '**/api/emails/*/rsvp*': (route) =>
          route.fulfill({
            status,
            contentType: 'application/json',
            body: JSON.stringify(code ? { error, code } : { error }),
          }),
        ...(accountErrors.length
          ? {
              '**/api/accounts': (route) =>
                route.fulfill({
                  status: 200,
                  contentType: 'application/json',
                  body: JSON.stringify({
                    accounts: [
                      { id: 'acct-1', email: 'me@example.com', provider: 'fastmail', authStatus: 'connected', isDefault: true },
                    ],
                    errors: accountErrors,
                  }),
                }),
            }
          : {}),
      },
    },
  });
  await page.goto('/');
  await expect(page.locator('#email-list .email-row')).toHaveCount(1, { timeout: 10_000 });
  await page.locator('#email-list .email-row').first().click();
  await expect(page.locator('.calendar-card')).toBeVisible({ timeout: 10_000 });
}

test('yq92: a 400 CalendarAuthUnconfigured RSVP failure renders the account-error banner with the actionable message', async ({ page }) => {
  await openInviteWithFailingRsvp(page, { status: 400, error: AUTH_MSG, code: 'calendar_auth_unconfigured' });
  await page.locator('#rsvp-accept').click();

  // The persistent, actionable surface: the account-error banner names the
  // fix (add an app password in Settings) — not just a transient toast.
  await expect(page.locator('#account-error-banner')).toBeVisible();
  await expect(page.locator('#account-error-details')).toContainText(AUTH_MSG);

  // The toast carries the human message, not the raw JSON error body.
  await expect(page.locator('#status-message')).toContainText(AUTH_MSG);
  await expect(page.locator('#status-message')).not.toContainText('{"error"');

  // The optimistic card update was reverted: no "You responded" label, no
  // highlighted button — the UI must not claim an RSVP that never sent.
  await expect(page.locator('#rsvp-status-label')).toBeHidden();
  await expect(page.locator('#rsvp-accept')).not.toHaveClass(/active/);
});

test('yq92: a 503 CalendarDiscoveryFailed RSVP failure renders the account-error banner with the actionable message', async ({ page }) => {
  await openInviteWithFailingRsvp(page, { status: 503, error: DISCOVERY_MSG, code: 'calendar_discovery_failed' });
  await page.locator('#rsvp-decline').click();

  await expect(page.locator('#account-error-banner')).toBeVisible();
  await expect(page.locator('#account-error-details')).toContainText(DISCOVERY_MSG);
  await expect(page.locator('#rsvp-status-label')).toBeHidden();
  await expect(page.locator('#rsvp-decline')).not.toHaveClass(/active/);
});

test('yq92: a generic 400 (no calendar code) stays toast-only — no account-error banner', async ({ page }) => {
  // Error::BadRequest also maps to 400 but is a one-off request problem, not
  // an account-config problem; promoting it to the persistent banner would
  // mislabel it. The gate is the body's machine-readable code, not the status.
  await openInviteWithFailingRsvp(page, { status: 400, error: 'bad request: something one-off' });
  await page.locator('#rsvp-accept').click();

  await expect(page.locator('#status-message')).toContainText('bad request: something one-off');
  await expect(page.locator('#account-error-banner')).toBeHidden();
});

test('yq92: an RSVP failure merges into the banner — other accounts\' errors are not clobbered', async ({ page }) => {
  // Boot with another account's error already on the banner. The RSVP
  // failure must ADD its line, not rebuild the banner from itself alone
  // (roborev 446): acct-2's unresolved problem stays visible.
  const bootError = { account: 'acct-2', provider: 'gmail', error: 'Not authorized — click Authorize' };
  await openInviteWithFailingRsvp(page, {
    status: 400,
    error: AUTH_MSG,
    code: 'calendar_auth_unconfigured',
    accountErrors: [bootError],
  });
  await expect(page.locator('#account-error-details')).toContainText(bootError.error);

  await page.locator('#rsvp-accept').click();

  await expect(page.locator('#account-error-details')).toContainText(AUTH_MSG);
  await expect(page.locator('#account-error-details')).toContainText(bootError.error);
});

test('yq92: a successful RSVP does NOT raise the account-error banner', async ({ page }) => {
  const email = inviteEmail({ method: 'REQUEST', summary: 'Sync' });
  await mockApi(page, {
    emails: [email],
    extra: {
      emailById: { [email.id]: email },
      routes: {
        '**/api/emails/*/rsvp*': (route) => {
          const updated = JSON.parse(JSON.stringify(email.calendarEvent));
          updated.user_rsvp_status = 'ACCEPTED';
          return route.fulfill({
            status: 200,
            contentType: 'application/json',
            body: JSON.stringify({ calendarEvent: updated }),
          });
        },
      },
    },
  });
  await page.goto('/');
  await expect(page.locator('#email-list .email-row')).toHaveCount(1, { timeout: 10_000 });
  await page.locator('#email-list .email-row').first().click();
  await expect(page.locator('.calendar-card')).toBeVisible({ timeout: 10_000 });

  await page.locator('#rsvp-accept').click();
  // Wait for the full success render — the banner assertion below is only
  // meaningful after the response path that could have raised it has run.
  await expect(page.locator('#rsvp-status-label')).toHaveText('You responded Accepted');
  await expect(page.locator('#account-error-banner')).toBeHidden();
});

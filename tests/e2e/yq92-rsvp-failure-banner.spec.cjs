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
// {"error": <constant>} with status 400 / 503.

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
// status + server-shaped error body.
async function openInviteWithFailingRsvp(page, { status, error }) {
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
            body: JSON.stringify({ error }),
          }),
      },
    },
  });
  await page.goto('/');
  await expect(page.locator('#email-list .email-row')).toHaveCount(1, { timeout: 10_000 });
  await page.locator('#email-list .email-row').first().click();
  await expect(page.locator('.calendar-card')).toBeVisible({ timeout: 10_000 });
}

test('yq92: a 400 CalendarAuthUnconfigured RSVP failure renders the account-error banner with the actionable message', async ({ page }) => {
  await openInviteWithFailingRsvp(page, { status: 400, error: AUTH_MSG });
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
  await openInviteWithFailingRsvp(page, { status: 503, error: DISCOVERY_MSG });
  await page.locator('#rsvp-decline').click();

  await expect(page.locator('#account-error-banner')).toBeVisible();
  await expect(page.locator('#account-error-details')).toContainText(DISCOVERY_MSG);
  await expect(page.locator('#rsvp-status-label')).toBeHidden();
  await expect(page.locator('#rsvp-decline')).not.toHaveClass(/active/);
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

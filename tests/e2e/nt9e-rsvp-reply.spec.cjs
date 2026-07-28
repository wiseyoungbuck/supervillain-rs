// E2E spec for kata nt9e: RSVP actions show only for METHOD:REQUEST.
//
// The contract tests (src/routes.rs app_js_shows_rsvp_actions_only_for_request_events
// + mobile_app_js_...) pin the code FORM — `const showActions = event.method === 'REQUEST'`
// on both platforms. This spec pins the OBSERVABLE behavior: after a real boot
// against a mocked invite email, the calendar card's RSVP buttons are visible
// for REQUEST and hidden for REPLY (the bug: REPLY used to show buttons that
// make no sense on someone else's answer).

const { test, expect } = require('@playwright/test');
const { mockApi, inviteEmail } = require('./fixtures.cjs');

async function openInvite(page, method) {
  const email = inviteEmail({ method, summary: method + ' invite' });
  await mockApi(page, {
    emails: [email],
    extra: { emailById: { [email.id]: email } },
  });
  await page.goto('/');
  // The email list renders, then we click the row to open the detail + calendar.
  await expect(page.locator('#email-list .email-row')).toHaveCount(1, { timeout: 10_000 });
  await page.locator('#email-list .email-row').first().click();
  // The calendar card is rendered when the detail's calendarEvent is present.
  await expect(page.locator('.calendar-card')).toBeVisible({ timeout: 10_000 });
}

// The RSVP action container holds the three buttons. The gate toggles display.
// We assert visibility rather than DOM presence — the buttons stay in the DOM
// (hidden via style.display='none') when actions are hidden, which is the
// observable "no buttons offered" the user sees.
function rsvpActions(page) {
  return page.locator('.calendar-actions');
}

test('nt9e: a REQUEST invite shows Accept/Maybe/Decline RSVP buttons', async ({ page }) => {
  await openInvite(page, 'REQUEST');
  await expect(rsvpActions(page)).toBeVisible();
  await expect(page.locator('#rsvp-accept')).toBeVisible();
  await expect(page.locator('#rsvp-maybe')).toBeVisible();
  await expect(page.locator('#rsvp-decline')).toBeVisible();
});

test('nt9e: a REPLY (an attendee\'s acceptance) shows NO RSVP buttons', async ({ page }) => {
  // This is the bug: before the fix, a REPLY offered Accept/Maybe/Decline on
  // someone else's answer. After, the REQUEST-only allowlist hides them.
  await openInvite(page, 'REPLY');
  await expect(rsvpActions(page)).toBeHidden();
});

test('nt9e: a CANCEL also shows no RSVP buttons (cancelled-banner logic stays separate)', async ({ page }) => {
  // CANCEL was already hidden before the ticket (the old denylist); the
  // allowlist must preserve that. The cancelled banner is a separate concern
  // not asserted here (it's a different element, .cal-cancelled).
  await openInvite(page, 'CANCEL');
  await expect(rsvpActions(page)).toBeHidden();
});

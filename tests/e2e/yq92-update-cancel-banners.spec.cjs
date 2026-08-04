// E2E spec for kata yq92 (gap 3): the reschedule (isUpdate) and CANCEL
// banners on the calendar card.
//
// The routes.rs logic that computes isUpdate (incremented SEQUENCE) and the
// contract tests that pin renderCalendarCard's code form already exist; this
// spec pins what the user actually SEES: a rescheduled invite shows the
// "Updated — please respond again" banner with the RSVP buttons reset, a
// METHOD:CANCEL shows the CANCELLED banner, and a plain invite shows neither.
// Banners are created/removed per render, so the cross-email test guards the
// "stale banner leaks onto the next invite" regression.

const { test, expect } = require('@playwright/test');
const { mockApi, inviteEmail } = require('./fixtures.cjs');

async function bootWithInvites(page, emails) {
  const byId = {};
  for (const e of emails) byId[e.id] = e;
  await mockApi(page, { emails, extra: { emailById: byId } });
  await page.goto('/');
  await expect(page.locator('#email-list .email-row')).toHaveCount(emails.length, { timeout: 10_000 });
}

async function openRow(page, index) {
  await page.locator('#email-list .email-row').nth(index).click();
  await expect(page.locator('.calendar-card')).toBeVisible({ timeout: 10_000 });
}

test('yq92: a rescheduled invite (isUpdate) renders the update banner with RSVP buttons reset', async ({ page }) => {
  // The user had ACCEPTED, then the organizer rescheduled: the server resets
  // user_rsvp_status to null (the fixture mirrors that), so the card must ask
  // again — banner up, no button highlighted, no stale "You responded" label.
  await bootWithInvites(page, [inviteEmail({ method: 'REQUEST', isUpdate: true, summary: 'Moved sync' })]);
  await openRow(page, 0);

  await expect(page.locator('.cal-updated')).toBeVisible();
  await expect(page.locator('.cal-updated')).toHaveText('Updated — please respond again');

  // Rescheduled ⇒ answer again: actions offered fresh.
  await expect(page.locator('.calendar-actions')).toBeVisible();
  await expect(page.locator('#rsvp-accept')).not.toHaveClass(/active/);
  await expect(page.locator('#rsvp-maybe')).not.toHaveClass(/active/);
  await expect(page.locator('#rsvp-decline')).not.toHaveClass(/active/);
  await expect(page.locator('#rsvp-status-label')).toBeHidden();

  // An update is not a cancellation — the destructive banner must not show.
  await expect(page.locator('.cal-cancelled')).toHaveCount(0);
});

test('yq92: a CANCEL invite renders the CANCELLED banner on a cancelled-styled card', async ({ page }) => {
  await bootWithInvites(page, [inviteEmail({ method: 'CANCEL', summary: 'Dead sync' })]);
  await openRow(page, 0);

  await expect(page.locator('.cal-cancelled')).toBeVisible();
  await expect(page.locator('.cal-cancelled')).toHaveText('CANCELLED');
  await expect(page.locator('.calendar-card')).toHaveClass(/cancelled/);
  await expect(page.locator('.cal-updated')).toHaveCount(0);
});

test('yq92: a plain REQUEST invite renders neither banner', async ({ page }) => {
  await bootWithInvites(page, [inviteEmail({ method: 'REQUEST', summary: 'Normal sync' })]);
  await openRow(page, 0);

  await expect(page.locator('.calendar-card')).toBeVisible();
  await expect(page.locator('.cal-updated')).toHaveCount(0);
  await expect(page.locator('.cal-cancelled')).toHaveCount(0);
  await expect(page.locator('.calendar-card')).not.toHaveClass(/cancelled/);
});

test('yq92: banners do not leak from one invite onto the next', async ({ page }) => {
  // The banner elements are inserted into the ONE shared calendar-card node
  // and removed on re-render. Open an updated invite, then a cancelled one,
  // then a plain one — each render must show exactly its own banner state.
  await bootWithInvites(page, [
    inviteEmail({ method: 'REQUEST', isUpdate: true, summary: 'Moved', emailId: 'e-upd' }),
    inviteEmail({ method: 'CANCEL', summary: 'Dead', emailId: 'e-cxl' }),
    inviteEmail({ method: 'REQUEST', summary: 'Plain', emailId: 'e-plain' }),
  ]);

  await openRow(page, 0);
  await expect(page.locator('.cal-updated')).toBeVisible();

  await page.keyboard.press('Escape');
  await openRow(page, 1);
  await expect(page.locator('.cal-cancelled')).toBeVisible();
  await expect(page.locator('.cal-updated')).toHaveCount(0);

  await page.keyboard.press('Escape');
  await openRow(page, 2);
  await expect(page.locator('.cal-updated')).toHaveCount(0);
  await expect(page.locator('.cal-cancelled')).toHaveCount(0);
});

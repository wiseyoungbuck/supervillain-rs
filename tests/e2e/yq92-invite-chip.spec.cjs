// E2E spec for kata yq92 (gap 5): the inbox-row calendar-invite chip (trbx).
//
// tests/invite_chip_test.cjs exercises the extracted renderInviteChip function
// in isolation; this spec pins the chip END TO END: the list fields the server
// emits (isInviteToMe/inviteMethod/inviteStatus/inviteIsUpdated) flow through
// a real boot into a rendered chip on the email row — and the chip flips
// optimistically after an RSVP without a list reload (rsvpToEvent's listItem
// update + renderEmailList).

const { test, expect } = require('@playwright/test');
const { mockApi, inviteEmail, ONE_EMAIL_LIST } = require('./fixtures.cjs');

async function bootList(page, emails, extra = {}) {
  const byId = {};
  for (const e of emails) byId[e.id] = e;
  await mockApi(page, { emails, extra: { emailById: byId, ...extra } });
  await page.goto('/');
  await expect(page.locator('#email-list .email-row')).toHaveCount(emails.length, { timeout: 10_000 });
}

function chip(page) {
  return page.locator('#email-list .email-row .email-invite');
}

test('yq92: an unanswered invite row shows the "Needs response" chip with a calendar icon', async ({ page }) => {
  await bootList(page, [inviteEmail({ method: 'REQUEST' })]);

  await expect(chip(page)).toHaveCount(1);
  await expect(chip(page)).toHaveClass(/email-invite--needs-action/);
  await expect(chip(page).locator('.email-invite-label')).toHaveText('Needs response');
  await expect(chip(page).locator('.email-invite-icon')).toHaveText('📅');
});

test('yq92: an already-accepted invite row shows the "Accepted" chip', async ({ page }) => {
  await bootList(page, [inviteEmail({ method: 'REQUEST', userStatus: 'ACCEPTED' })]);

  await expect(chip(page)).toHaveClass(/email-invite--accepted/);
  await expect(chip(page).locator('.email-invite-label')).toHaveText('Accepted');
});

test('yq92: a rescheduled invite row shows the "Updated" chip regardless of the old answer', async ({ page }) => {
  // The user had accepted, then the organizer moved the event: "Updated"
  // must win over the stale ACCEPTED so the row says why it needs attention.
  await bootList(page, [inviteEmail({ method: 'REQUEST', userStatus: 'ACCEPTED', isUpdate: true })]);

  await expect(chip(page)).toHaveClass(/email-invite--updated/);
  await expect(chip(page).locator('.email-invite-label')).toHaveText('Updated');
});

test('yq92: non-invite rows show no chip (REPLY traffic and plain mail)', async ({ page }) => {
  // A REPLY is someone else's answer landing in the organizer's inbox — not
  // an invite to me — and a plain email has no calendar at all.
  await bootList(page, [
    inviteEmail({ method: 'REPLY', emailId: 'e-reply' }),
    ...ONE_EMAIL_LIST,
  ]);

  await expect(chip(page)).toHaveCount(0);
});

test('yq92: after an RSVP the row chip flips to the answer without a list reload', async ({ page }) => {
  const email = inviteEmail({ method: 'REQUEST' });
  await bootList(page, [email], {
    routes: {
      '**/api/emails/*/rsvp*': (route) => {
        const updated = JSON.parse(JSON.stringify(email.calendarEvent));
        updated.user_rsvp_status = route.request().postDataJSON().status;
        return route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify({ calendarEvent: updated }),
        });
      },
    },
  });
  await expect(chip(page)).toHaveClass(/email-invite--needs-action/);

  // Open, accept, return to the list.
  await page.locator('#email-list .email-row').first().click();
  await expect(page.locator('.calendar-card')).toBeVisible({ timeout: 10_000 });
  await page.locator('#rsvp-accept').click();
  await expect(page.locator('#rsvp-status-label')).toHaveText('You responded Accepted');
  await page.keyboard.press('Escape');

  // The chip reflects the new answer from the optimistic listItem update —
  // the /api/emails list route was only ever fetched at boot.
  await expect(chip(page)).toHaveClass(/email-invite--accepted/);
  await expect(chip(page).locator('.email-invite-label')).toHaveText('Accepted');
});

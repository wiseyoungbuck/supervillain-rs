// E2E spec for kata yq92 (gap 1): the RSVP click flow.
//
// The loopback tests pin the provider round-trip and the routes.rs contract
// tests pin the code form; this spec pins the OBSERVABLE browser flow: a click
// on Accept/Maybe/Decline fires POST /api/emails/{id}/rsvp with the correct
// {status} body, and the calendar card's rendered response state (the
// "You responded ..." label + the highlighted button) updates from the
// server's {calendarEvent} response. Plus the no-op guard: clicking the
// already-selected status must NOT re-send.

const { test, expect } = require('@playwright/test');
const { mockApi, inviteEmail } = require('./fixtures.cjs');

// Boot with one REQUEST invite and a mocked rsvp route that echoes the
// requested status back the way the real handler does (src/routes.rs rsvp):
// { calendarEvent: <event with user_rsvp_status + the 'me' attendee updated> }.
// Returns the array the route handler pushes every captured request into.
async function openInviteWithRsvpMock(page, { userStatus = null } = {}) {
  const email = inviteEmail({ method: 'REQUEST', summary: 'Sync', userStatus });
  const rsvpRequests = [];
  await mockApi(page, {
    emails: [email],
    extra: {
      emailById: { [email.id]: email },
      routes: {
        // Trailing * so the glob still matches with the ?account= query the
        // shared api client appends to account-scoped routes.
        '**/api/emails/*/rsvp*': (route) => {
          const body = route.request().postDataJSON();
          rsvpRequests.push({ url: route.request().url(), body });
          const updated = JSON.parse(JSON.stringify(email.calendarEvent));
          updated.user_rsvp_status = body.status;
          for (const a of updated.attendees) {
            if (a.email === 'me@example.com') a.status = body.status;
          }
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
  return rsvpRequests;
}

// One click → one POST with the ICS status for that button → rendered label +
// button highlight reflect the server response. Parametrized over the three
// buttons because the behavior contract is identical; each case still runs as
// its own test() so a failure names the button.
for (const [buttonId, icsStatus, label] of [
  ['#rsvp-accept', 'ACCEPTED', 'Accepted'],
  ['#rsvp-maybe', 'TENTATIVE', 'Maybe'],
  ['#rsvp-decline', 'DECLINED', 'Declined'],
]) {
  test(`yq92: clicking ${label} POSTs {status: ${icsStatus}} and renders the responded state`, async ({ page }) => {
    const rsvpRequests = await openInviteWithRsvpMock(page);

    const posted = page.waitForRequest(
      (r) => r.method() === 'POST' && r.url().includes('/rsvp'),
    );
    await page.locator(buttonId).click();
    const req = await posted;

    // The request contract: the route carries the email id, the body carries
    // the ICS status — nothing else (the server derives the attendee).
    expect(req.url()).toContain('/api/emails/e-cal/rsvp');
    expect(req.postDataJSON()).toEqual({ status: icsStatus });

    // The rendered response state, after the mocked server response lands:
    // the label reports the answer and only the clicked button is highlighted.
    await expect(page.locator('#rsvp-status-label')).toHaveText(`You responded ${label}`);
    await expect(page.locator(buttonId)).toHaveClass(/active/);
    for (const other of ['#rsvp-accept', '#rsvp-maybe', '#rsvp-decline']) {
      if (other !== buttonId) {
        await expect(page.locator(other)).not.toHaveClass(/active/);
      }
    }

    expect(rsvpRequests).toHaveLength(1);
  });
}

test('yq92: clicking the already-selected status is a no-op (no duplicate POST)', async ({ page }) => {
  // An invite the user already accepted: the card opens with Accept
  // highlighted and the label showing.
  const rsvpRequests = await openInviteWithRsvpMock(page, { userStatus: 'ACCEPTED' });
  await expect(page.locator('#rsvp-status-label')).toHaveText('You responded Accepted');
  await expect(page.locator('#rsvp-accept')).toHaveClass(/active/);

  // Clicking Accept again must not re-send (rsvpToEvent's same-status guard).
  // Clicking Decline afterwards must send — awaiting that request gives a
  // deterministic "the Accept click had its chance" barrier, so the
  // zero-requests-from-Accept assertion needs no arbitrary sleep.
  await page.locator('#rsvp-accept').click();
  const posted = page.waitForRequest(
    (r) => r.method() === 'POST' && r.url().includes('/rsvp'),
  );
  await page.locator('#rsvp-decline').click();
  await posted;

  expect(rsvpRequests).toHaveLength(1);
  expect(rsvpRequests[0].body).toEqual({ status: 'DECLINED' });
  await expect(page.locator('#rsvp-status-label')).toHaveText('You responded Declined');
});

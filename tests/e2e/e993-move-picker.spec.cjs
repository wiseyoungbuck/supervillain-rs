// E2E for kata e993: the Move to Folder picker end-to-end — press `v` on a
// list row, type-to-filter the mailbox list, Enter, and the email files into
// the chosen mailbox: the row leaves the list and the real POST
// /api/emails/{id}/move carries the chosen mailbox_id. The default fixture
// mailboxes include a markup-payload name and a quote-bearing id, so this
// also proves the picker renders hostile names as text in a real browser.

const { test, expect } = require('@playwright/test');
const { mockApi, ONE_EMAIL_LIST, MAILBOXES_WITH_XSS } = require('./fixtures.cjs');

const SECOND_EMAIL = {
  ...ONE_EMAIL_LIST[0],
  id: 'e-2',
  subject: 'Second email',
  from: [{ name: 'Other', email: 'other@example.com' }],
  receivedAt: new Date(Date.now() - 3_600_000).toISOString(),
};

const EMAILS = [ONE_EMAIL_LIST[0], SECOND_EMAIL];

// The fixture mailbox with the attribute-breakout id (1p0d) — picking it
// exercises the picker's escapeAttr path with a real DOM round-trip.
const EVIL_ID_MAILBOX = MAILBOXES_WITH_XSS.find((m) => m.name === 'Evil Id Mailbox');

test('e993: v → filter → Enter files the email and POSTs the mailbox id', async ({ page }) => {
  let moveBody = null;
  await mockApi(page, {
    emails: EMAILS,
    extra: {
      routes: {
        '**/api/emails/*/move**': (route) => {
          moveBody = route.request().postDataJSON();
          return route.fulfill({ status: 204 });
        },
        // Serve list refills (offset > 0) empty so the mock cannot race the
        // suppression window and resurrect the moved row (same guard as the
        // map4 undo spec).
        '**/api/emails?**': (route) => {
          const url = new URL(route.request().url());
          const offset = Number(url.searchParams.get('offset') || '0');
          return route.fulfill({
            status: 200,
            contentType: 'application/json',
            body: JSON.stringify(offset > 0 ? [] : EMAILS),
          });
        },
      },
    },
  });
  await page.goto('/');
  const rows = page.locator('#email-list .email-row');
  await expect(rows).toHaveCount(2, { timeout: 10_000 });

  await page.keyboard.press('v');
  await expect(page.locator('#move-modal')).toBeVisible();
  // The XSS-named fixture mailbox must be literal text, never a live element.
  await expect(page.locator('#move-modal img')).toHaveCount(0);

  await page.locator('#move-filter').fill('evil');
  await expect(page.locator('#move-picker-list .move-picker-item')).toHaveCount(1);
  await page.keyboard.press('Enter');

  await expect(page.locator('#move-modal')).toBeHidden();
  await expect(rows).toHaveCount(1);
  await expect(page.locator('#email-list')).not.toContainText('Hello');
  await expect.poll(() => moveBody).not.toBeNull();
  expect(moveBody.mailbox_id).toBe(EVIL_ID_MAILBOX.id);
});

test('e993: Escape cancels the picker without moving anything', async ({ page }) => {
  let moved = false;
  await mockApi(page, {
    emails: EMAILS,
    extra: {
      routes: {
        '**/api/emails/*/move**': (route) => {
          moved = true;
          return route.fulfill({ status: 204 });
        },
      },
    },
  });
  await page.goto('/');
  const rows = page.locator('#email-list .email-row');
  await expect(rows).toHaveCount(2, { timeout: 10_000 });

  await page.keyboard.press('v');
  await expect(page.locator('#move-modal')).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(page.locator('#move-modal')).toBeHidden();
  await expect(rows).toHaveCount(2);
  expect(moved).toBe(false);
});

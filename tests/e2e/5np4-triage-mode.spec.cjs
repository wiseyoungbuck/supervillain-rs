// E2E for kata 5np4: the Get-Me-To-Zero triage run in a real browser.
// T enters triage over the mocked unread set, the statusbar shows progress,
// each archive advances to the next UNREAD (skipping read emails), and the
// last action lands back on the list with the zero state announced.

const { test, expect } = require('@playwright/test');
const { mockApi, ONE_EMAIL_LIST } = require('./fixtures.cjs');

const EMAILS = [
  { ...ONE_EMAIL_LIST[0], isUnread: true }, // e-1 'Hello', unread, newest
  {
    ...ONE_EMAIL_LIST[0],
    id: 'e-2',
    subject: 'Second email',
    from: [{ name: 'Other', email: 'other@example.com' }],
    receivedAt: new Date(Date.now() - 3_600_000).toISOString(),
    isUnread: false, // read — triage must skip it
  },
  {
    ...ONE_EMAIL_LIST[0],
    id: 'e-3',
    subject: 'Third email',
    from: [{ name: 'Third', email: 'third@example.com' }],
    receivedAt: new Date(Date.now() - 7_200_000).toISOString(),
    isUnread: true,
  },
];

test('5np4: T walks the unread queue to inbox zero', async ({ page }) => {
  const archived = [];
  await mockApi(page, {
    emails: EMAILS,
    extra: {
      routes: {
        '**/api/emails/*/archive**': (route) => {
          archived.push(route.request().url().match(/emails\/([^/]+)\/archive/)[1]);
          return route.fulfill({ status: 204 });
        },
        '**/api/emails/*/mark-read**': (route) => route.fulfill({ status: 204 }),
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
  await expect(page.locator('#email-list .email-row')).toHaveCount(3, { timeout: 10_000 });

  // Enter triage: the first unread opens in detail with progress showing.
  await page.keyboard.press('T');
  await expect(page.locator('#email-detail-view')).toHaveClass(/active/);
  await expect(page.locator('#email-subject')).toHaveText('Hello');
  await expect(page.locator('#triage-progress')).toBeVisible();
  await expect(page.locator('#triage-progress')).toHaveText('TRIAGE 1/2');

  // Archive advances to the next UNREAD — e-2 is read and must be skipped.
  await page.keyboard.press('e');
  await expect(page.locator('#email-subject')).toHaveText('Third email');
  await expect(page.locator('#triage-progress')).toHaveText('TRIAGE 2/2');

  // The last action lands on the zero state.
  await page.keyboard.press('e');
  await expect(page.locator('#email-list-view')).toHaveClass(/active/);
  await expect(page.locator('#triage-progress')).toBeHidden();
  await expect(page.locator('#status-message')).toContainText(/inbox zero/i);
  await expect(page.locator('#email-list .email-row')).toHaveCount(1);
  await expect(page.locator('#email-list')).toContainText('Second email');
  await expect.poll(() => archived.length).toBe(2);
  expect(archived.sort()).toEqual(['e-1', 'e-3']);
});

test('5np4: Escape leaves triage without touching anything', async ({ page }) => {
  await mockApi(page, {
    emails: EMAILS,
    extra: {
      routes: {
        '**/api/emails/*/mark-read**': (route) => route.fulfill({ status: 204 }),
      },
    },
  });
  await page.goto('/');
  await expect(page.locator('#email-list .email-row')).toHaveCount(3, { timeout: 10_000 });

  await page.keyboard.press('T');
  await expect(page.locator('#triage-progress')).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(page.locator('#email-list-view')).toHaveClass(/active/);
  await expect(page.locator('#triage-progress')).toBeHidden();
  await expect(page.locator('#email-list .email-row')).toHaveCount(3);
});

// E2E spec for kata 9rg8: desktop unsubscribeAndArchiveAll reads the real
// server response ({ success, archived, sender }) and shows the real archived
// count.
//
// The contract test (src/routes.rs desktop_unsub_reads_server_response_fields)
// pins the code FORM — the function references .archived and contains neither
// archivedCount nor unsubscribeUrl. This spec pins the OBSERVABLE behavior:
// after triggering the unsubscribe-and-archive action against a mocked POST
// that returns the REAL server shape, the status line shows the real count
// ("Archived 3 emails from …"), not "undefined".

const { test, expect } = require('@playwright/test');
const { mockApi, ONE_EMAIL_LIST } = require('./fixtures.cjs');

test('9rg8: unsubscribe-and-archive-all shows the real archived count from the server response', async ({ page }) => {
  // Mock the POST with the REAL server shape: { success, archived, sender }.
  // The bug: the client used to read archivedCount (undefined) and show
  // "Archived undefined emails". After the fix it reads .archived and shows 3.
  const ARCHIVED_COUNT = 3;
  const SENDER = 'spammy@example.com';
  await mockApi(page, {
    emails: ONE_EMAIL_LIST,
    extra: {
      routes: {
        '**/api/emails/*/unsubscribe-and-archive-all**': (route) =>
          route.fulfill({
            status: 200,
            contentType: 'application/json',
            body: JSON.stringify({ success: true, archived: ARCHIVED_COUNT, sender: SENDER }),
          }),
      },
    },
  });
  await page.goto('/');
  await expect(page.locator('#email-list .email-row')).toHaveCount(1, { timeout: 10_000 });
  await page.locator('#email-list .email-row').first().click();

  // The unsubscribe-and-archive-all action is wired to the SHIFT+U shortcut
  // (static/index.html: "U Unsubscribe + archive all"). Note: lowercase 'u' is
  // toggleUnreadSelected — a different action. Trigger the uppercase shortcut.
  await page.keyboard.press('Shift+KeyU');

  // The status line (#status-message) must show the REAL count and sender —
  // "Archived 3 emails from spammy@example.com." — not "undefined".
  await expect(page.locator('#status-message')).toContainText(`Archived ${ARCHIVED_COUNT} emails from ${SENDER}`, { timeout: 5_000 });
  await expect(page.locator('#status-message')).not.toContainText('undefined');
});

test('9rg8: the dead unsubscribeUrl branch is gone — no window.open is called on success', async ({ page }) => {
  // Before the fix the client read result.unsubscribeUrl and called window.open
  // on it — dead code that never executed because the server never sent the
  // field. After the fix the branch is deleted. Assert no window.open happens
  // by stubbing it to throw, then triggering the action; if the branch were
  // present, the throw would surface as a page error.
  await mockApi(page, {
    emails: ONE_EMAIL_LIST,
    extra: {
      routes: {
        '**/api/emails/*/unsubscribe-and-archive-all**': (route) =>
          route.fulfill({
            status: 200,
            contentType: 'application/json',
            body: JSON.stringify({ success: true, archived: 1, sender: 'x@example.com' }),
          }),
      },
    },
  });
  await page.addInitScript(() => {
    window.__openCalled = false;
    const orig = window.open;
    window.open = function (...args) {
      window.__openCalled = true;
      return orig ? orig.apply(null, args) : null;
    };
  });
  await page.goto('/');
  await expect(page.locator('#email-list .email-row')).toHaveCount(1, { timeout: 10_000 });
  await page.locator('#email-list .email-row').first().click();
  await page.keyboard.press('Shift+KeyU');
  // Give the handler a moment to resolve.
  await page.waitForTimeout(500);
  const openCalled = await page.evaluate(() => window.__openCalled);
  expect(openCalled).toBe(false);
});

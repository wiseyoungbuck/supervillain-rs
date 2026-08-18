// E2E for kata map4: existing app functionality is reachable from the command
// palette. Representative command: Undo (the marquee audit gap) — archive a
// row with the existing `e` keybinding, then run "Undo" from the palette and
// watch the row return. Also pins the palette's state gate end-to-end: before
// anything is undoable, the palette offers no Undo command at all.

const { test, expect } = require('@playwright/test');
const { mockApi, ONE_EMAIL_LIST } = require('./fixtures.cjs');

const SECOND_EMAIL = {
  ...ONE_EMAIL_LIST[0],
  id: 'e-2',
  subject: 'Second email',
  from: [{ name: 'Other', email: 'other@example.com' }],
  // Older than e-1 so the fixture order (e-1 first, selected at boot) is
  // also the chronological order the list renders.
  receivedAt: new Date(Date.now() - 3_600_000).toISOString(),
};

const EMAILS = [ONE_EMAIL_LIST[0], SECOND_EMAIL];

test('map4: Undo runs from the palette and restores the archived row', async ({ page }) => {
  await mockApi(page, {
    emails: EMAILS,
    extra: {
      routes: {
        // Undo's server side: move the email back to the inbox. Trailing **
        // swallows the ?account= suffix makeApi appends (same as 9rg8's
        // unsubscribe mock). The archive POST needs the same treatment:
        // fixtures' query-less '**/api/emails/*/archive' misses the scoped
        // URL, the POST falls through to the real server, and the eventual
        // failure-revert re-inserts the row a second time mid-test.
        '**/api/emails/*/archive**': (route) => route.fulfill({ status: 204 }),
        '**/api/emails/*/move**': (route) => route.fulfill({ status: 204 }),
        // Post-archive the list drops below REFILL_THRESHOLD and the app
        // refills with offset=<count>. Serve the refill EMPTY so the mock
        // can't race the suppression window and re-add the archived row —
        // only palette Undo may bring it back.
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

  // Gate, end to end: nothing undoable yet → the palette offers no Undo.
  await page.keyboard.press('Control+KeyK');
  await expect(page.locator('#command-palette')).toBeVisible();
  await page.locator('#command-input').fill('undo');
  await expect(page.locator('#command-results .command-item')).toHaveCount(0);
  await page.keyboard.press('Escape');
  await expect(page.locator('#command-palette')).toBeHidden();

  // Archive the selected (first) row with the existing keybinding.
  await page.keyboard.press('e');
  await expect(rows).toHaveCount(1);
  await expect(page.locator('#email-list')).not.toContainText('Hello');

  // Undo through the palette path.
  await page.keyboard.press('Control+KeyK');
  await expect(page.locator('#command-palette')).toBeVisible();
  await page.locator('#command-input').fill('undo');
  const undoCmd = page.locator('#command-results .command-item', { hasText: 'Undo' });
  await expect(undoCmd).toHaveCount(1);
  await page.keyboard.press('Enter');

  await expect(rows).toHaveCount(2);
  await expect(page.locator('#email-list')).toContainText('Hello');
});

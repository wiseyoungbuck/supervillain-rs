// E2E for kata mtqp: Share Availability — the compose picker fetches
// /api/calendar/free-slots and inserts the formatted slot text into the
// compose BODY at the cursor. Reachable via Ctrl+Shift+H and the palette.

const { test, expect } = require('@playwright/test');
const { mockApi } = require('./fixtures.cjs');

// Pin the browser timezone: the inserted text renders slot times and the
// timezone parenthetical in local time.
test.use({ timezoneId: 'UTC' });

const FREE_SLOTS = {
  slots: [
    { start: '2026-08-18T09:00:00Z', end: '2026-08-18T09:30:00Z' },
    { start: '2026-08-18T09:30:00Z', end: '2026-08-18T10:00:00Z' },
    { start: '2026-08-19T11:00:00Z', end: '2026-08-19T11:30:00Z' },
  ],
  errors: [],
};

test('mtqp: picker inserts formatted free slots into the compose body at the cursor', async ({ page }) => {
  await mockApi(page, {
    extra: {
      routes: {
        '**/api/calendar/free-slots**': (route) =>
          route.fulfill({
            status: 200,
            contentType: 'application/json',
            body: JSON.stringify(FREE_SLOTS),
          }),
      },
    },
  });
  await page.goto('/');
  await expect(page.locator('#email-list .email-row').first()).toBeVisible({ timeout: 10_000 });

  // Open compose and put a greeting in the body; the cursor rests at the end.
  await page.keyboard.press('c');
  await expect(page.locator('#compose-view')).toBeVisible();
  const body = page.locator('#compose-body');
  await body.click();
  await body.type('Hi,\n\n');

  // The palette offers the command from compose.
  await page.keyboard.press('Control+KeyK');
  await expect(page.locator('#command-palette')).toBeVisible();
  await page.locator('#command-input').fill('avail');
  await expect(
    page.locator('#command-results .command-item', { hasText: 'Share Availability' })
  ).toBeVisible();
  await page.keyboard.press('Escape');
  await body.click();

  // Ctrl+Shift+H opens the picker; Enter inserts with the defaults.
  await page.keyboard.press('Control+Shift+KeyH');
  const modal = page.locator('#avail-modal');
  await expect(modal).toBeVisible();
  await page.keyboard.press('Enter');
  await expect(modal).toBeHidden();

  const expected =
    'Hi,\n\n' +
    'Would any of these times work?\n\n' +
    '- Tue, Aug 18: 09:00–10:00\n' +
    '- Wed, Aug 19: 11:00–11:30\n\n' +
    '(times in UTC)';
  await expect(body).toHaveValue(expected);

  // The insertion is body-text only: still on the compose screen, nothing sent.
  await expect(page.locator('#compose-view')).toBeVisible();
});

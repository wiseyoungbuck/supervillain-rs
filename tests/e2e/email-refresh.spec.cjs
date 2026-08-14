// Observable refresh behavior: an explicit desktop refresh must request
// provider-fresh data and replace an old visible row. The server-side unit
// tests cover cache bypass/update; this spec pins the user-facing DOM result.

const { test, expect } = require('@playwright/test');
const { mockApi } = require('./fixtures.cjs');

const OLD_EMAIL = {
  id: 'e-old',
  subject: 'Yesterday',
  from: [{ name: 'Sender', email: 'sender@example.com' }],
  to: [{ name: 'Me', email: 'me@example.com' }],
  receivedAt: '2026-08-01T10:00:00.000Z',
  isUnread: false,
  preview: 'old preview',
  account: 'acct-1',
};

const NEW_EMAIL = {
  ...OLD_EMAIL,
  id: 'e-new',
  subject: 'Just arrived',
  receivedAt: '2026-08-01T11:00:00.000Z',
  preview: 'fresh preview',
};

test('email-refresh: R replaces a cached-looking row with fresh mail', async ({ page }) => {
  const refreshUrls = [];
  await mockApi(page, {
    emails: [OLD_EMAIL],
    extra: {
      routes: {
        '**/api/emails?**': (route) => {
          const url = new URL(route.request().url());
          // Playwright globs treat '?' as a wildcard, so keep this override
          // from shadowing /api/emails/:id detail and mutation routes.
          if (url.pathname !== '/api/emails') return route.fallback();
          if (url.searchParams.get('refresh') === 'true') {
            refreshUrls.push(url);
            return route.fulfill({
              status: 200,
              contentType: 'application/json',
              body: JSON.stringify([NEW_EMAIL]),
            });
          }
          return route.fulfill({
            status: 200,
            contentType: 'application/json',
            body: JSON.stringify([OLD_EMAIL]),
          });
        },
      },
    },
  });

  await page.goto('/');
  await expect(page.locator('#email-list .email-row')).toHaveCount(1, { timeout: 10_000 });
  await expect(page.locator('#email-list')).toContainText('Yesterday');

  await page.keyboard.press('R');

  await expect(page.locator('#email-list')).toContainText('Just arrived');
  await expect(page.locator('#email-list')).not.toContainText('Yesterday');
  expect(refreshUrls).toHaveLength(1);
});

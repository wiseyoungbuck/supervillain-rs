// E2E for kata pakx: bulk selection in a real browser — x/j/x builds a
// two-email selection (bar shows the count and rows carry the marker), `e`
// archives the batch through the existing per-email endpoint (one POST per
// id), and a single `z` undoes the whole batch back into the list.

const { test, expect } = require('@playwright/test');
const { mockApi, ONE_EMAIL_LIST } = require('./fixtures.cjs');

const EMAILS = [
  ONE_EMAIL_LIST[0], // e-1 'Hello', newest
  {
    ...ONE_EMAIL_LIST[0],
    id: 'e-2',
    subject: 'Second email',
    from: [{ name: 'Other', email: 'other@example.com' }],
    receivedAt: new Date(Date.now() - 3_600_000).toISOString(),
  },
  {
    ...ONE_EMAIL_LIST[0],
    id: 'e-3',
    subject: 'Third email',
    from: [{ name: 'Third', email: 'third@example.com' }],
    receivedAt: new Date(Date.now() - 7_200_000).toISOString(),
  },
];

test('pakx: select two with x, archive as a batch, undo restores both', async ({ page }) => {
  const archived = [];
  await mockApi(page, {
    emails: EMAILS,
    extra: {
      routes: {
        '**/api/emails/*/archive**': (route) => {
          archived.push(route.request().url().match(/emails\/([^/]+)\/archive/)[1]);
          return route.fulfill({ status: 204 });
        },
        '**/api/emails/*/move**': (route) => route.fulfill({ status: 204 }),
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
  await expect(rows).toHaveCount(3, { timeout: 10_000 });

  // x selects the first row; j x adds the second.
  await page.keyboard.press('x');
  await expect(page.locator('#email-list .email-row.bulk-selected')).toHaveCount(1);
  await page.keyboard.press('j');
  await page.keyboard.press('x');
  await expect(page.locator('#email-list .email-row.bulk-selected')).toHaveCount(2);
  await expect(page.locator('#bulk-bar')).toBeVisible();
  await expect(page.locator('#bulk-bar')).toContainText('2 selected');

  // e archives the whole selection.
  await page.keyboard.press('e');
  await expect(rows).toHaveCount(1);
  await expect(page.locator('#email-list')).toContainText('Third email');
  await expect(page.locator('#bulk-bar')).toBeHidden();
  await expect.poll(() => archived.length).toBe(2);
  expect(archived.sort()).toEqual(['e-1', 'e-2']);

  // One z restores the whole batch.
  await page.keyboard.press('z');
  await expect(rows).toHaveCount(3);
  await expect(page.locator('#email-list')).toContainText('Hello');
  await expect(page.locator('#email-list')).toContainText('Second email');
});

test('pakx: Escape clears the selection without acting', async ({ page }) => {
  await mockApi(page, { emails: EMAILS });
  await page.goto('/');
  const rows = page.locator('#email-list .email-row');
  await expect(rows).toHaveCount(3, { timeout: 10_000 });

  await page.keyboard.press('x');
  await page.keyboard.press('j');
  await page.keyboard.press('x');
  await expect(page.locator('#email-list .email-row.bulk-selected')).toHaveCount(2);
  await page.keyboard.press('Escape');
  await expect(page.locator('#email-list .email-row.bulk-selected')).toHaveCount(0);
  await expect(page.locator('#bulk-bar')).toBeHidden();
  await expect(rows).toHaveCount(3);
});

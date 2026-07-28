// E2E spec for kata qknk (MOBILE shell): branded splash covers the cold-boot
// gap on /mobile/ too.
//
// The desktop spec (tests/e2e/qknk-boot-splash.spec.cjs) pins the behavior on
// the desktop shell at /. Mobile is a SEPARATE shell at /mobile/ with its own
// index.html and its own app.js (type="module", implicitly deferred). Its
// loadAccounts has the same cold-boot gap, and the same fix shipped a
// #boot-splash to its initial HTML with hideBootSplash() called from its
// loadAccounts success + init()'s catch. This spec pins the OBSERVABLE
// behavior on the mobile shell — the contract test
// (mobile_app_js_load_accounts_dismisses_boot_splash) only pins the code shape,
// not that the splash actually shows and dismisses in a real mobile boot.
//
// Same delayed-/api/accounts technique as the desktop spec: mockApi fulfills
// instantly, so the gap is near-zero; override /api/accounts with a delayed
// handler so the splash is observable for a determinate window.

const { test, expect } = require('@playwright/test');
const { mockApi, ACCOUNTS } = require('./fixtures.cjs');

const ACCOUNTS_DELAY_MS = 500;

function delayedAccountsHandler(route) {
  return new Promise((resolve) =>
    setTimeout(() => {
      route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify(ACCOUNTS),
      });
      resolve();
    }, ACCOUNTS_DELAY_MS)
  );
}

test('qknk mobile: the splash is visible on first paint, before /api/accounts resolves', async ({ page }) => {
  await mockApi(page, {
    extra: {
      routes: {
        '**/api/accounts': delayedAccountsHandler,
      },
    },
  });

  // /mobile/ is the mobile shell. domcontentloaded returns the instant the
  // deferred module app.js has fired the (delayed) accounts fetch — before the
  // 500ms window has elapsed, so the splash is still showing.
  await page.goto('/mobile/', { waitUntil: 'domcontentloaded' });

  // The mobile shell's #boot-splash is in its initial HTML and paints on the
  // first frame; the delayed accounts fetch has NOT resolved yet, so
  // hideBootSplash() has not run. #boot-splash must be visible. (Before the
  // fix #boot-splash doesn't exist in mobile's HTML, so this fails — RED.)
  await expect(page.locator('#boot-splash')).toBeVisible();
});

test('qknk mobile: the splash dismisses when boot resolves', async ({ page }) => {
  await mockApi(page, {
    extra: {
      routes: {
        '**/api/accounts': delayedAccountsHandler,
      },
    },
  });

  await page.goto('/mobile/', { waitUntil: 'domcontentloaded' });

  // Establish the splash shipped, so the dismissal assertion has teeth.
  await expect(page.locator('#boot-splash')).toBeVisible();

  // Let boot resolve: the delayed accounts fetch completes, selectAccount runs,
  // and the mobile email list renders. The first .email-row is the
  // "content is ready" signal — it only appears after hideBootSplash() ran at
  // the end of mobile loadAccounts.
  await expect(page.locator('#email-list .email-row')).toHaveCount(1, { timeout: 10_000 });

  // hideBootSplash() fades #boot-splash out (~150ms) then removes it from the
  // DOM, so once content has rendered the splash element must be gone.
  // toHaveCount(0) auto-retries past the fade-out window.
  await expect(page.locator('#boot-splash')).toHaveCount(0);
});

// E2E spec for kata qknk: a branded splash/loading state covers the cold-boot
// gap between first paint and loadAccounts() resolving.
//
// The contract tests (src/routes.rs index_html_has_boot_splash /
// mobile_html_has_boot_splash / app_js_load_accounts_dismisses_boot_splash /
// mobile_app_js_load_accounts_dismisses_boot_splash) pin the code SHAPE — both
// shells ship a #boot-splash div and both loadAccounts paths call
// hideBootSplash(). This spec pins the OBSERVABLE behavior in a real browser:
// the splash is visible on the very first frame (before /api/accounts
// resolves), and it dismisses once boot resolves and the mailbox list renders.
//
// Timing: mockApi fulfills /api/accounts instantly, so the gap the splash
// covers is near-zero and the splash may dismiss before the test can observe
// it. The honest fix is to OVERRIDE /api/accounts with a delayed handler so the
// splash is observable for a determinate window — the splash exists precisely
// to show during the accounts fetch, and delaying the fetch makes that window
// testable. mockApi sets up every other boot route (mailboxes, emails,
// identities, etc.) so the rest of boot proceeds normally once accounts
// resolve; the delayed accounts route is registered LAST (via extra.routes) so
// Playwright's LIFO route matching lets it take precedence over mockApi's
// instant accounts handler.

const { test, expect } = require('@playwright/test');
const { mockApi, ACCOUNTS } = require('./fixtures.cjs');

// Delay (ms) the /api/accounts handler sleeps before fulfilling. Long enough
// that the splash is deterministically observable after page.goto returns at
// DOMContentLoaded (the fetch is fired by deferred app.js just before
// DOMContentLoaded, so goto returns ~0ms into this window), short enough to
// keep the suite fast.
const ACCOUNTS_DELAY_MS = 500;

// A /api/accounts handler that fulfills the REAL accounts fixture (so boot
// proceeds normally after the delay) but only after sleeping. Registered via
// mockApi's extra.routes so it overrides mockApi's instant accounts handler.
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

test('qknk: the splash is visible on first paint, before /api/accounts resolves', async ({ page }) => {
  // Mock every boot route, then override /api/accounts with a delayed
  // fulfillment so the splash stays on screen for a determinate window.
  await mockApi(page, {
    extra: {
      routes: {
        '**/api/accounts': delayedAccountsHandler,
      },
    },
  });

  // domcontentloaded returns the instant deferred app.js has run and fired the
  // (delayed) accounts fetch — before the 500ms window has elapsed — so the
  // splash is still showing when goto returns.
  await page.goto('/', { waitUntil: 'domcontentloaded' });

  // The splash is in the initial html and paints on the first frame; the
  // delayed accounts fetch has NOT resolved yet, so hideBootSplash() has not
  // run. #boot-splash must be visible. (Before the fix #boot-splash doesn't
  // exist, so this assertion fails — the RED test.)
  await expect(page.locator('#boot-splash')).toBeVisible();
});

test('qknk: the splash dismisses when boot resolves', async ({ page }) => {
  await mockApi(page, {
    extra: {
      routes: {
        '**/api/accounts': delayedAccountsHandler,
      },
    },
  });

  // domcontentloaded returns the instant deferred app.js has fired the
  // (delayed) accounts fetch — before the 500ms window has elapsed.
  await page.goto('/', { waitUntil: 'domcontentloaded' });

  // First confirm the splash is showing — establishes that it shipped, so the
  // dismissal assertion below has teeth (otherwise "count 0" would pass
  // trivially if the splash never existed). Before the fix this fails for the
  // right reason: #boot-splash is not in the DOM.
  await expect(page.locator('#boot-splash')).toBeVisible();

  // Now let boot resolve: the delayed accounts fetch completes, selectAccount
  // runs, and the mailbox list renders. The first email row is the
  // "content is ready" signal — it only appears after hideBootSplash() has
  // been called at the end of loadAccounts.
  await expect(page.locator('#email-list .email-row')).toHaveCount(1, { timeout: 10_000 });

  // hideBootSplash() fades #boot-splash out (~150ms) then removes it from the
  // DOM, so once content has rendered the splash element must be gone.
  // toHaveCount(0) auto-retries past the fade-out window, so this is robust to
  // the exact removal timing. Guards the "splash never dismisses" regression.
  await expect(page.locator('#boot-splash')).toHaveCount(0);

  // Steady-state pin: the deploy banner must NOT be showing after a normal
  // boot. The fixtures deliberately do not mock /api/build-id — the real
  // server's id matches the <meta name="build-id"> the same binary stamped
  // into the shell. A past fixture mocked it to a literal, so every spec ran
  // in a permanent "deploy pending" UI state (banner up, poll dead, layout
  // shifted). This assertion fails if such a mock ever comes back.
  await expect(page.locator('#deploy-banner')).toHaveClass(/hidden/);
});

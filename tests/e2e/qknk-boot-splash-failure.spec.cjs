// E2E spec for kata qknk (FAILURE path): the splash never hangs as a permanent
// mask when boot fails.
//
// The success-path specs (qknk-boot-splash.spec.cjs + qknk-boot-splash-mobile.spec.cjs)
// pin that the splash shows and dismisses on a SUCCESSFUL boot. This spec pins
// the ticket's explicit failure criterion: "Given boot fails (an account error
// at startup), when loadAccounts throws, then the splash dismisses and the
// error banner shows — the splash never hangs as a permanent mask."
//
// The contract test (app_js_load_accounts_dismisses_boot_splash) only asserts
// hideBootSplash() appears somewhere in the loadAccounts slice — it's
// satisfied by the success-path call alone, so the catch-path call could be
// deleted and the contract test would still pass. This behavioral spec is the
// honest pin: it makes /api/accounts actually fail, then asserts the splash is
// gone (dismissed on catch) and an error is visible.
//
// Both shells: desktop loadAccounts has its own try/catch that calls
// hideBootSplash() + showStatus('Failed to load accounts…'); mobile init()'s
// try/catch around loadAccounts calls hideBootSplash() + showError/showStatus.

const { test, expect } = require('@playwright/test');
const { mockApi } = require('./fixtures.cjs');

// Mock /api/accounts to 500 — the boot fetch fails, driving the catch path.
function accountsFailsHandler(route) {
  return route.fulfill({
    status: 500,
    contentType: 'text/plain',
    body: 'internal server error (e2e mock)',
  });
}

test('qknk desktop: a boot failure dismisses the splash — it never hangs as a permanent mask', async ({ page }) => {
  await mockApi(page, {
    extra: {
      routes: {
        '**/api/accounts': accountsFailsHandler,
      },
    },
  });

  await page.goto('/', { waitUntil: 'domcontentloaded' });

  // The splash shows on first paint (same as the success-path spec).
  await expect(page.locator('#boot-splash')).toBeVisible();

  // Now let the failing accounts fetch resolve. loadAccounts's catch runs
  // hideBootSplash() + showStatus('Failed to load accounts…', 'error'). The
  // splash must be gone — NOT hung as a permanent mask over the error.
  // toHaveCount(0) auto-retries past the ~150ms fade-out.
  await expect(page.locator('#boot-splash')).toHaveCount(0, { timeout: 10_000 });

  // And an error must be visible — the status line shows the failure message.
  // This is the "the error banner/status is visible instead of hanging on the
  // splash forever" contract. The exact wording is the app's; assert it
  // mentions "accounts" so a regression that swallows the error silently is
  // caught.
  await expect(page.locator('#status-message')).toContainText(/account/i, { timeout: 5_000 });
});

test('qknk mobile: a boot failure dismisses the splash — it never hangs as a permanent mask', async ({ page }) => {
  await mockApi(page, {
    extra: {
      routes: {
        '**/api/accounts': accountsFailsHandler,
      },
    },
  });

  await page.goto('/mobile/', { waitUntil: 'domcontentloaded' });

  await expect(page.locator('#boot-splash')).toBeVisible();

  // Mobile init()'s try/catch around loadAccounts runs hideBootSplash() on the
  // 500, then showError/showStatus. The splash must be gone, not hung.
  await expect(page.locator('#boot-splash')).toHaveCount(0, { timeout: 10_000 });

  // Mobile surfaces the error via #error-toast (showError) — the catch's
  // showError('Load accounts', …) populates it. Assert it's visible and
  // mentions the failure so a regression that leaves a blank screen (splash
  // gone, no error) is caught. (Strict mode: target #error-toast specifically,
  // not a broad selector that also matches the status bar.)
  await expect(page.locator('#error-toast')).toBeVisible({ timeout: 5_000 });
  await expect(page.locator('#error-toast')).toContainText(/account|server/i, { timeout: 5_000 });
});

// E2E regression for kata fhtz: user-controlled split names are command
// labels, not command-palette markup. The Rust contract test pins the escape
// helper call sites and tests/palette_test.cjs exercises the real renderer;
// this spec proves the browser parses the payload as text rather than a live
// element with an executable handler.

const { test, expect } = require('@playwright/test');
const { mockApi } = require('./fixtures.cjs');

const SPLIT_NAME = '<img src=x onerror="window.__xss_palette=1">';

const XSS_SPLIT = {
  id: `split\"double'single`,
  name: SPLIT_NAME,
  filters: [],
};

test.beforeEach(async ({ page }) => {
  await mockApi(page, { splits: [XSS_SPLIT] });
  await page.goto('/');
  // Wait for loadSplits + the inbox selection to converge before opening the
  // context-aware list palette.
  await expect(page.locator('#split-tabs .split-tab')).toHaveCount(2, { timeout: 10_000 });
  await page.keyboard.press('Control+KeyK');
  await expect(page.locator('#command-palette')).toBeVisible();
});

test('fhtz: a markup-named split is literal command text, not a live element', async ({ page }) => {
  const command = page.locator('#command-results .command-item', {
    hasText: `Delete Split: ${SPLIT_NAME}`,
  });
  await expect(command).toHaveCount(1);
  await expect(command).toContainText(SPLIT_NAME);

  await expect(page.locator('#command-results img')).toHaveCount(0);
  expect(await page.evaluate(() => typeof window.__xss_palette !== 'undefined')).toBe(false);
  expect(await command.evaluate((el) => el.hasAttribute('onerror'))).toBe(false);
});

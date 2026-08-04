// E2E spec for kata 6rqw (behavior 2): the inbox filter tabs render split
// counts.
//
// renderSplitTabs (static/app.js) hides the whole bar when
// state.splits.length === 0, else renders one tab per split plus the "All"
// tab, and appends a .split-count badge only when state.splitCounts[id] is
// non-null. The counts come from GET /api/split-counts (mocked here via
// mockApi's splitCounts option — the live finding was Fastmail returning
// {"aristoi":215,...} and Gmail {}). Three pins, one per test:
//   (a) account with splits → bar visible, every tab shows its mocked count;
//   (b) account with no splits → bar hidden (the length===0 guard);
//   (c) a split whose count is null/absent → tab renders WITHOUT a badge.

const { test, expect } = require('@playwright/test');
const { mockApi } = require('./fixtures.cjs');

// Two splits shaped like the server's SplitInbox (src/types.rs): the app
// reads id, name, (optionally) icon — and filters, which getRecipientBadge
// iterates while rendering the email list. The server always serializes
// filters (serde default [], no skip), so the fixture must carry it too:
// omitting it TypeErrors the list render and leaves the pane on "Loading".
const SPLITS = [
  { id: 's-work', name: 'Work', filters: [] },
  { id: 's-news', name: 'News', filters: [] },
];

const tabNamed = (page, name) =>
  page.locator('#split-tabs .split-tab').filter({ has: page.locator('.split-name', { hasText: name }) });

test('kata 6rqw: account with splits shows the tab bar with a count badge per tab', async ({ page }) => {
  await mockApi(page, {
    splits: SPLITS,
    splitCounts: { all: 42, 's-work': 7, 's-news': 3 },
  });
  await page.goto('/');
  await expect(page.locator('#email-list .email-row')).toHaveCount(1, { timeout: 10_000 });

  await expect(page.locator('#split-tabs')).toBeVisible();
  // "All" first, then one tab per configured split.
  await expect(page.locator('#split-tabs .split-tab')).toHaveCount(3);
  await expect(tabNamed(page, 'All').locator('.split-count')).toHaveText('42');
  await expect(tabNamed(page, 'Work').locator('.split-count')).toHaveText('7');
  await expect(tabNamed(page, 'News').locator('.split-count')).toHaveText('3');
});

test('kata 6rqw: account with no splits hides the tab bar entirely', async ({ page }) => {
  await mockApi(page, { splits: [] });
  await page.goto('/');
  // Boot fully (list rendered) before asserting the bar stayed hidden, so a
  // late renderSplitTabs can't sneak in after the assertion.
  await expect(page.locator('#email-list .email-row')).toHaveCount(1, { timeout: 10_000 });

  await expect(page.locator('#split-tabs')).toBeHidden();
  await expect(page.locator('#split-tabs .split-tab')).toHaveCount(0);
});

test('kata 6rqw: a split with a null/absent count renders its tab without a badge', async ({ page }) => {
  await mockApi(page, {
    splits: SPLITS,
    // 's-work' has a count; 's-news' is explicitly null (the server can emit
    // either null or omit the key — the app guards with `count != null`).
    splitCounts: { all: 42, 's-work': 7, 's-news': null },
  });
  await page.goto('/');
  await expect(page.locator('#email-list .email-row')).toHaveCount(1, { timeout: 10_000 });

  await expect(page.locator('#split-tabs')).toBeVisible();
  // The counted tab shows its badge — proves counts have landed, so the
  // no-badge assertion below isn't just racing the fetch.
  await expect(tabNamed(page, 'Work').locator('.split-count')).toHaveText('7');
  // The null-count tab is present but badge-free.
  await expect(tabNamed(page, 'News')).toBeVisible();
  await expect(tabNamed(page, 'News').locator('.split-count')).toHaveCount(0);
});

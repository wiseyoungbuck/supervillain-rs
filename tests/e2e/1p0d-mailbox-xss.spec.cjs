// E2E spec for kata 1p0d: mailbox/folder name + id rendered safely (XSS).
//
// The contract test (src/routes.rs app_js_render_mailboxes_escapes_name_and_id)
// pins the code FORM — that renderMailboxes calls escapeHtml(m.name) and
// escapeAttr(m.id). This spec pins the OBSERVABLE behavior the contract test
// can't see: after a real boot against mocked /api/mailboxes carrying XSS
// payloads, the rendered #mailbox-list contains the payload as TEXT, not as a
// live element, and no onerror/onmouseover handler ever fires (we assert a
// sentinel the handlers would set is never set).

const { test, expect } = require('@playwright/test');
const { mockApi, MAILBOXES_WITH_XSS } = require('./fixtures.cjs');

test.beforeEach(async ({ page }) => {
  await mockApi(page, { mailboxes: MAILBOXES_WITH_XSS });
  await page.goto('/');
  // Wait for the mailbox list to render (loadAccounts → selectAccount →
  // loadMailboxes → renderMailboxes). The inbox item is always present.
  // Excludes the synthetic Reminders item (kata dd0d) — it is app-generated,
  // not attacker-controlled, and would make the provider-mailbox count 4.
  await expect(page.locator('#mailbox-list .mailbox-item:not(.reminders-item)')).toHaveCount(3, { timeout: 10_000 });
});

test('1p0d: a mailbox name containing an <img onerror> payload renders as text, not a live element', async ({ page }) => {
  // The payload mailbox is the second item (inbox sorts first by role).
  const payloadItem = page.locator('#mailbox-list .mailbox-item', { hasText: 'onerror' }).first();
  await expect(payloadItem).toBeVisible();
  // No live <img> element is rendered inside the mailbox list — the payload
  // must have been entity-encoded so the parser saw text, not a tag.
  await expect(page.locator('#mailbox-list img')).toHaveCount(0);
  // And the onerror handler must never have executed: the sentinel it would set
  // on window is absent. This is the real attack gate — script execution, not
  // just presence of the string.
  const xssFired = await page.evaluate(() => typeof window.__xss_mailbox !== 'undefined');
  expect(xssFired).toBe(false);
});

test('1p0d: a mailbox id containing a quote-breakout payload does not escape the data-id attribute', async ({ page }) => {
  // The evil-id mailbox item is identified by its visible label.
  const evilItem = page.locator('#mailbox-list .mailbox-item', { hasText: 'Evil Id Mailbox' }).first();
  await expect(evilItem).toBeVisible();
  // The data-id attribute must contain the payload entity-encoded (no raw
  // onmouseover attribute on the element). If escapeAttr were absent, the
  // browser would have parsed the injected onmouseover as a real attribute.
  const onmouseoverPresent = await evilItem.evaluate((el) => el.hasAttribute('onmouseover'));
  expect(onmouseoverPresent).toBe(false);
  // And the handler must not have fired.
  const attrXssFired = await page.evaluate(() => typeof window.__xss_attr !== 'undefined');
  expect(attrXssFired).toBe(false);
});

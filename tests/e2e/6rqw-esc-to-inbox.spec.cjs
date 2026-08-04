// E2E spec for kata 6rqw (behavior 1): Escape returns to the inbox list from
// the email detail view.
//
// The keyboard path is handleKeyDown → handleNormalModeKey → case 'Escape':
// showView('list') (static/app.js). The logic was intact when the regression
// was reported, so the basic test is a regression pin. The second test pins
// the fragile part: the sandboxed email-body iframe swallows keyboard focus
// when clicked (it's cross-origin by design), and the window-blur focus-bounce
// (static/app.js, `window.addEventListener('blur', ...)`) is what keeps Escape
// working after a click into the email body. Nothing is stubbed: a real email
// opens, the real iframe gets the click, the real key is pressed.

const { test, expect } = require('@playwright/test');
const { mockApi } = require('./fixtures.cjs');

// An email with an htmlBody so the detail view renders the sandboxed
// .email-iframe (text-only bodies skip the iframe entirely, which would
// make the focus-bounce test vacuous).
const HTML_EMAIL = {
  id: 'e-html',
  subject: 'HTML newsletter',
  from: [{ name: 'Sender', email: 'sender@example.com' }],
  to: [{ name: 'Me', email: 'me@example.com' }],
  receivedAt: new Date().toISOString(),
  isUnread: false,
  preview: 'body preview',
  account: 'acct-1',
  htmlBody: '<html><body><p>Some HTML content long enough to click into.</p></body></html>',
};

// Boot, open the one email, and wait until the detail view is the active one.
async function openEmail(page) {
  await mockApi(page, { emails: [HTML_EMAIL], extra: { emailById: { [HTML_EMAIL.id]: HTML_EMAIL } } });
  await page.goto('/');
  await expect(page.locator('#email-list .email-row')).toHaveCount(1, { timeout: 10_000 });
  await page.locator('#email-list .email-row').first().click();
  await expect(page.locator('#email-detail-view')).toBeVisible({ timeout: 10_000 });
  await expect(page.locator('#email-list-view')).toBeHidden();
}

test('kata 6rqw: Escape in the detail view returns to the inbox list', async ({ page }) => {
  await openEmail(page);

  await page.keyboard.press('Escape');

  await expect(page.locator('#email-list-view')).toBeVisible();
  await expect(page.locator('#email-detail-view')).toBeHidden();
});

test('kata 6rqw: Escape still returns to the list after clicking into the email-body iframe', async ({ page }) => {
  await openEmail(page);

  // The real sandboxed iframe must be there — clicking it is the whole point.
  const iframe = page.locator('#email-detail-view iframe.email-iframe');
  await expect(iframe).toBeVisible();
  await iframe.click();

  // The click moves focus into the cross-origin iframe; the window-blur
  // focus-bounce (setTimeout 0) must hand it back to the parent document,
  // otherwise the keydown below dies inside the iframe and never reaches
  // handleKeyDown. Wait for the bounce to land rather than racing it — if
  // the bounce is broken, this times out and the test goes RED here, naming
  // the actual failure instead of a generic "list never appeared".
  await page.waitForFunction(
    () => !document.activeElement?.classList?.contains('email-iframe'),
    undefined,
    { timeout: 5_000 },
  );

  await page.keyboard.press('Escape');

  await expect(page.locator('#email-list-view')).toBeVisible();
  await expect(page.locator('#email-detail-view')).toBeHidden();
});

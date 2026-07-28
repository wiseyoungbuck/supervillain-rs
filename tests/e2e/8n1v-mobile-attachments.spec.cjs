// E2E spec for kata 8n1v: mobile failed-upload attachments block send.
//
// Sub-issue 1 (the headline) is automated here end-to-end via the REAL failure
// path: pick a file in the compose file input, mock /api/upload to return 500,
// the app's uploadComposeAttachment sets the attachment status to 'error', then
// pressing Send is blocked — no POST /api/emails/send fires and an error toast
// shows. Before the fix, readyAttachments filtered to status==='ready' and the
// email sent without the failed attachment silently.
//
// Driving the real upload-failure path (not page.evaluate reaching into module
// scope) matters: mobile app.js is type="module", so `state` is module-private
// and not visible to page.evaluate. The file-input + mocked-500 path is the
// honest way to land an error attachment in that module-private state.
//
// Sub-issues 2 and 3 are NOT automated here, by design:
//   - Pull-to-refresh race (sub-2): faithful automation needs real touch-event
//     sequences (touchstart/move/end with the recognizer's geometry gates).
//     The contract test (src/routes.rs mobile_pull_to_refresh_aborts_an_in_flight_list_load_first)
//     pins the code form — abortListLoad() before loadEmails() — which is the
//     honest automatable layer; the gesture itself stays a manual repro.
//   - downloadAllAttachments Safari user-activation (sub-3): the contract test
//     (mobile_download_all_attachments_keeps_user_activation) pins that clicks
//     are NOT deferred via setTimeout (synchronous forEach(a => a.click())),
//     which is the code-form guarantee that preserves user activation. The
//     actual Safari-only "opens N tabs" behavior is webkit-specific and stays
//     a manual repro; this suite runs chromium only.
//
// Mobile is a separate shell at /mobile/ with its own app.js (a module), so
// this spec targets /mobile/ and mocks the same /api/* routes.

const { test, expect } = require('@playwright/test');
const { mockApi, ONE_EMAIL_LIST } = require('./fixtures.cjs');

async function openComposeAndFailAnUpload(page) {
  // Mock /api/upload to fail — this is what lands an attachment in status='error'
  // through the real uploadComposeAttachment path.
  await mockApi(page, {
    emails: ONE_EMAIL_LIST,
    extra: {
      routes: {
        '**/api/upload**': (route) =>
          route.fulfill({ status: 500, contentType: 'text/plain', body: 'upload failed (e2e mock)' }),
      },
    },
  });
  await page.goto('/mobile/');
  await expect(page.locator('#email-list .email-row')).toHaveCount(1, { timeout: 10_000 });
  await page.locator('#compose-btn').click();
  await expect(page.locator('#compose-screen')).toBeVisible({ timeout: 5_000 });

  // Pick a file via the real file input. Playwright's setInputFiles drives the
  // change event, addComposeAttachment runs, the mocked 500 makes
  // uploadComposeAttachment set status='error'. Wait for the error chip.
  await page.locator('#compose-file-input').setInputFiles({
    name: 'broken.pdf',
    mimeType: 'application/pdf',
    buffer: Buffer.from('%PDF-1.4 fake'),
  });
  // The error chip is a span.compose-attachment-chip.error inside
  // #compose-attachments-list (see renderComposeAttachments in mobile app.js).
  // Wait for it so we know the upload-failure path completed before Send.
  await expect(page.locator('#compose-attachments-list .compose-attachment-chip.error').first()).toBeVisible({ timeout: 5_000 });
}

test('8n1v: a failed-upload attachment blocks send — no POST /api/emails/send fires', async ({ page }) => {
  await openComposeAndFailAnUpload(page);

  // Sentry: if the send path proceeds past the error gate, it POSTs to
  // /api/emails/send. Intercept and record. After the fix the gate returns
  // early and the route is never hit.
  let sendAttempted = false;
  await page.route('**/api/emails/send', (route) => {
    sendAttempted = true;
    return route.fulfill({ status: 200, contentType: 'application/json', body: '{}' });
  });

  // Fill minimal compose fields so the ONLY thing blocking send is the error
  // attachment (a missing recipient would block for a different reason).
  await page.locator('#compose-to').fill('someone@example.com');
  await page.locator('#compose-subject').fill('test');
  await page.locator('#compose-body').fill('body');
  await page.locator('#compose-send-btn').click();

  // Give the async send path a moment to either fire or bail at the error gate.
  await page.waitForTimeout(500);
  expect(sendAttempted).toBe(false);
});

test('8n1v: blocking send on a failed attachment surfaces an error mentioning the attachment', async ({ page }) => {
  await openComposeAndFailAnUpload(page);
  await page.locator('#compose-to').fill('someone@example.com');
  await page.locator('#compose-subject').fill('test');
  await page.locator('#compose-body').fill('body');
  await page.locator('#compose-send-btn').click();

  // The upload-failure toast (from the mocked 500) OR the send-blocked toast
  // mentions the attachment. Assert SOMETHING error-flavored is visible that
  // references the attachment, so a regression that silently drops the failed
  // attachment (sends without it, no toast) is caught. The exact wording is the
  // app's; we match on "attachment" case-insensitively.
  await expect(
    page.locator('[class*="error"], #error-toast, .toast, .banner').filter({ hasText: /attachment/i })
  ).toBeVisible({ timeout: 5_000 });
});

// E2E for kata vj6k (Undo Send) + acag (Send Later): a plain Ctrl+Enter send
// defers via the backend queue and raises a countdown toast whose Undo click
// cancels the queued send and restores the compose draft; the Send Later
// palette command schedules an explicit time.

const { test, expect } = require('@playwright/test');
const { mockApi } = require('./fixtures.cjs');

// A queued-send record as DELETE /api/scheduled-sends/{id} returns it —
// submission included, so the client can rebuild the draft.
const QUEUED_RECORD = {
  id: 'q-1',
  account_id: 'acct-1',
  from_addr: 'me@example.com',
  send_at: new Date(Date.now() + 10_000).toISOString(),
  queued_at: new Date().toISOString(),
  submission: {
    to: ['dest@example.com'],
    cc: [],
    subject: 'Deferred hello',
    text_body: 'later gator',
    in_reply_to: null,
    attachments: [],
  },
  attempts: 0,
};

function sendRoutes(sentBodies) {
  return {
    '**/api/emails/send**': (route) => {
      const body = route.request().postDataJSON();
      sentBodies.push(body);
      return route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify(
          body.send_at
            ? { success: true, scheduled: true, id: 'q-1', sendAt: body.send_at }
            : { success: true },
        ),
      });
    },
    '**/api/scheduled-sends/q-1**': (route) =>
      route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify(QUEUED_RECORD),
      }),
  };
}

async function composeDeferredHello(page) {
  await page.goto('/');
  await expect(page.locator('#email-list .email-row')).toHaveCount(1, { timeout: 10_000 });
  await page.keyboard.press('c');
  await expect(page.locator('#compose-view')).toBeVisible();
  await page.locator('#compose-to').fill('dest@example.com');
  await page.locator('#compose-subject').fill('Deferred hello');
  await page.locator('#compose-body').fill('later gator');
}

test('vj6k: send raises the undo countdown; Undo restores the draft', async ({ page }) => {
  const sentBodies = [];
  await mockApi(page, { extra: { routes: sendRoutes(sentBodies) } });
  await composeDeferredHello(page);

  await page.keyboard.press('Control+Enter');

  // The POST deferred via the backend queue (send_at in the body)…
  await expect(page.locator('#send-undo-toast')).toBeVisible({ timeout: 5_000 });
  expect(sentBodies.length).toBe(1);
  expect(sentBodies[0].send_at).toBeTruthy();
  const delta = new Date(sentBodies[0].send_at).getTime() - Date.now();
  expect(delta).toBeGreaterThan(3_000);
  expect(delta).toBeLessThan(61_000);

  // …with a live countdown, back on the list view (compose finished).
  await expect(page.locator('#send-undo-message')).toHaveText(/Sending in \d+s/);
  await expect(page.locator('#email-list-view')).toBeVisible();

  // Undo: the queued send is cancelled and the draft comes back.
  await page.locator('#send-undo-button').click();
  await expect(page.locator('#send-undo-toast')).toBeHidden();
  await expect(page.locator('#compose-view')).toBeVisible({ timeout: 5_000 });
  await expect(page.locator('#compose-to')).toHaveValue('dest@example.com');
  await expect(page.locator('#compose-subject')).toHaveValue('Deferred hello');
  await expect(page.locator('#compose-body')).toHaveValue('later gator');
});

test('acag: Send Later from the palette schedules an explicit time', async ({ page }) => {
  const sentBodies = [];
  await mockApi(page, { extra: { routes: sendRoutes(sentBodies) } });
  await composeDeferredHello(page);

  await page.keyboard.press('Control+KeyK');
  await expect(page.locator('#command-palette')).toBeVisible();
  await page.locator('#command-input').fill('send later');
  await page.keyboard.press('Enter');

  await expect(page.locator('#send-later-modal')).toBeVisible();
  await page.locator('[data-send-later-quick="hour"]').click();

  await expect(page.locator('#status-message')).toContainText('Scheduled for', { timeout: 5_000 });
  expect(sentBodies.length).toBe(1);
  const delta = new Date(sentBodies[0].send_at).getTime() - Date.now();
  expect(delta).toBeGreaterThan(55 * 60_000);
  expect(delta).toBeLessThan(65 * 60_000);
  // An explicit schedule is not an undo window — no countdown toast.
  await expect(page.locator('#send-undo-toast')).toBeHidden();
});

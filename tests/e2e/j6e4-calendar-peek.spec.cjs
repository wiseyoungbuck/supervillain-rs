// E2E for kata j6e4: the calendar peek renders events from
// GET /api/calendar/events in a day/week pane alongside email — toggled by
// the C key and reachable from the command palette; w switches to the week
// grid; Escape closes.

const { test, expect } = require('@playwright/test');
const { mockApi } = require('./fixtures.cjs');

// Pin the browser timezone so "today" and the mocked events' day columns
// can't drift apart across the host's local midnight.
test.use({ timezoneId: 'UTC' });

// Events for "today" (the peek's opening anchor), shaped like the server's
// camelCase RangeEvents.
function todayIso(hour) {
  const now = new Date();
  const day = now.toISOString().slice(0, 10);
  return `${day}T${String(hour).padStart(2, '0')}:00:00Z`;
}

const PEEK_EVENTS = {
  events: [
    {
      uid: 't-today',
      summary: 'Design review',
      start: todayIso(10),
      end: todayIso(11),
      allDay: false,
      location: 'Room 1',
      account: 'acct-1',
    },
    {
      uid: 'ad-today',
      summary: 'Offsite',
      start: `${new Date().toISOString().slice(0, 10)}T00:00:00Z`,
      end: `${new Date(Date.now() + 86_400_000).toISOString().slice(0, 10)}T00:00:00Z`,
      allDay: true,
      location: null,
      account: 'acct-1',
    },
  ],
  errors: [],
};

test('j6e4: C toggles the peek, events render, w switches to week, palette reopens it', async ({ page }) => {
  await mockApi(page, {
    extra: {
      routes: {
        '**/api/calendar/events**': (route) =>
          route.fulfill({
            status: 200,
            contentType: 'application/json',
            body: JSON.stringify(PEEK_EVENTS),
          }),
      },
    },
  });
  await page.goto('/');
  await expect(page.locator('#email-list .email-row').first()).toBeVisible({ timeout: 10_000 });

  const peek = page.locator('#calendar-peek');
  await expect(peek).toBeHidden();

  // C opens the day view with today's events fetched and placed.
  await page.keyboard.press('C');
  await expect(peek).toBeVisible();
  await expect(peek.locator('.peek-day')).toHaveCount(1);
  const timedEvent = peek.locator('.peek-timed .peek-event[data-uid="t-today"]');
  await expect(timedEvent).toBeVisible();
  await expect(timedEvent).toContainText('Design review');
  await expect(peek.locator('.peek-allday .peek-event[data-uid="ad-today"]')).toContainText('Offsite');

  // w switches to the week grid: seven day columns, events still placed.
  await page.keyboard.press('w');
  await expect(peek.locator('.peek-day')).toHaveCount(7);
  await expect(peek.locator('.peek-event[data-uid="t-today"]')).toBeVisible();

  // Escape closes the peek (and only the peek — still on the list).
  await page.keyboard.press('Escape');
  await expect(peek).toBeHidden();
  await expect(page.locator('#email-list .email-row').first()).toBeVisible();

  // The command palette offers the same toggle.
  await page.keyboard.press('Control+KeyK');
  await expect(page.locator('#command-palette')).toBeVisible();
  await page.locator('#command-input').fill('calendar');
  await expect(
    page.locator('#command-results .command-item', { hasText: 'Calendar Peek' })
  ).toBeVisible();
  await page.keyboard.press('Enter');
  await expect(peek).toBeVisible();
});

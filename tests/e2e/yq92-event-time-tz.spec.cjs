// E2E spec for kata yq92 (gap 4): the invite card renders the event time as a
// wall-clock time in the CONFIGURED primary timezone, not raw UTC.
//
// The fixture's dtstart is the fixed instant 2026-08-01T15:00:00Z. With the
// primary mocked to America/Chicago (CDT, UTC-5 on that date) the rendered
// string must say 10:00 AM CDT — through the real app.js formatter
// (formatEventTimeMultiTz → Intl), end to end. Additional display zones render
// as secondary lines. Determinism: the instant is fixed in the fixture and the
// zones are fixed in the mock, so the output doesn't depend on the machine's
// clock or timezone — no Date freezing needed.
//
// The mocked /api/timezone body mirrors src/timezone.rs ResolvedTimezones.

const { test, expect } = require('@playwright/test');
const { mockApi, inviteEmail } = require('./fixtures.cjs');

function timezoneRoute(display) {
  return (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        primary: display[0],
        display,
        system: display[0],
        system_changed: false,
        use_system: false,
      }),
    });
}

async function openInviteWithZones(page, display) {
  const email = inviteEmail({ method: 'REQUEST', summary: 'Sync' });
  await mockApi(page, {
    emails: [email],
    extra: {
      emailById: { [email.id]: email },
      routes: {
        // Registered last, so it wins over mockApi's default '{}' handler
        // (Playwright matches routes LIFO). The exact-path glob doesn't
        // swallow /api/timezone/zones.
        '**/api/timezone': timezoneRoute(display),
      },
    },
  });
  await page.goto('/');
  await expect(page.locator('#email-list .email-row')).toHaveCount(1, { timeout: 10_000 });
  await page.locator('#email-list .email-row').first().click();
  await expect(page.locator('.calendar-card')).toBeVisible({ timeout: 10_000 });
}

test('yq92: the event time renders as the primary timezone\'s wall clock, not raw UTC', async ({ page }) => {
  await openInviteWithZones(page, ['America/Chicago']);

  const primary = page.locator('#cal-datetime .event-time.primary');
  await expect(primary).toBeVisible();

  // 15:00Z on 2026-08-01 is 10:00 AM CDT; the card shows date + start – end.
  // \s tolerates the narrow no-break space some ICU versions put before AM.
  await expect(primary).toContainText(/Aug 1/);
  await expect(primary).toContainText(/10:00\sAM/);
  await expect(primary).toContainText(/CDT/);
  await expect(primary).toContainText(/11:00\sAM/); // dtend, same day → time only

  // The raw UTC wall clock must NOT be what's shown.
  await expect(primary).not.toContainText(/3:00\sPM/);
  await expect(primary).not.toContainText('15:00');
});

test('yq92: additional display zones render as secondary time lines', async ({ page }) => {
  await openInviteWithZones(page, ['America/Chicago', 'America/New_York']);

  // Primary line unchanged...
  await expect(page.locator('#cal-datetime .event-time.primary')).toContainText(/10:00\sAM/);

  // ...and the extra zone renders its own wall clock: 15:00Z = 11:00 AM EDT.
  const secondary = page.locator('#cal-datetime .event-time.secondary');
  await expect(secondary).toHaveCount(1);
  await expect(secondary).toContainText(/11:00\sAM/);
  await expect(secondary).toContainText(/EDT/);
});

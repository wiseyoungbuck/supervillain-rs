// Removes the temp XDG_CONFIG_HOME the webServer booted against (created in
// playwright.config.cjs tempConfigDir()). Playwright runs globalTeardown on
// success AND failure; a hung/SIGKILLed run is covered by the stale-dir sweep
// on the next config load. The sv-e2e- guard makes an unset/mangled env var
// a no-op rather than an rm of something we didn't create.
const fs = require('node:fs');
const path = require('node:path');

module.exports = async () => {
  const dir = process.env.SV_E2E_CONFIG_DIR;
  if (!dir || !path.basename(dir).startsWith('sv-e2e-')) return;
  try {
    fs.rmSync(dir, { recursive: true, force: true });
  } catch {
    // Best effort — the stale sweep on the next run is the backstop.
  }
};

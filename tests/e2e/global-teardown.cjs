// Removes the temp XDG_CONFIG_HOME the webServer booted against (created in
// playwright.config.cjs tempConfigDir()). Playwright runs globalTeardown on
// success AND failure; a hung/SIGKILLed run is covered by the stale-dir sweep
// on the next config load.
//
// The dir is read from the config object Playwright passes in — the same
// webServer.env the server actually booted with — rather than a parallel
// env-var side channel that could drift from it. The sv-e2e- guard makes a
// missing/mangled value a no-op rather than an rm of something we didn't
// create.
const fs = require('node:fs');
const path = require('node:path');

module.exports = async (config) => {
  const servers = [config.webServer].flat().filter(Boolean);
  for (const server of servers) {
    const dir = server.env && server.env.XDG_CONFIG_HOME;
    if (!dir || !path.basename(dir).startsWith('sv-e2e-')) continue;
    try {
      fs.rmSync(dir, { recursive: true, force: true });
    } catch {
      // Best effort — the stale sweep on the next run is the backstop.
    }
  }
};

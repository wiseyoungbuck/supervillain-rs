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
  // Single-server form on purpose: FullConfig.webServer is null when the
  // user config supplies an ARRAY of webServers (Playwright keeps those in
  // an internal field), so an array-handling loop here would be illusory.
  // If this suite ever grows multiple webServers, their dirs fall through
  // to the stale sweep until this teardown learns the new shape.
  const dir = config.webServer && config.webServer.env
    && config.webServer.env.XDG_CONFIG_HOME;
  if (!dir || !path.basename(dir).startsWith('sv-e2e-')) return;
  try {
    fs.rmSync(dir, { recursive: true, force: true });
  } catch {
    // Best effort — the stale sweep on the next run is the backstop.
  }
};

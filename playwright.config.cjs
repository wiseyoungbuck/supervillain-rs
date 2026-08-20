// Playwright config for the supervillain e2e suite (kata 1p0d/9rg8/nt9e/8n1v).
//
// Design (honest about the trade-off):
//   - The server is the REAL `supervillain` binary (the workspace target/debug
//     build when present, so specs test HEAD), booted against a temp
//     XDG_CONFIG_HOME so no real accounts/tokens are touched. It serves the
//     real embedded index.html / app.js / style.css — the exact bytes a user
//     gets — so the specs assert against the real rendered DOM.
//   - Every /api/* call is mocked at the network layer via page.route() in each
//     spec's fixture. No provider is ever hit; the data is deterministic
//     fixtures. This is the only way to test the 4 tickets' behavior without
//     live Gmail/Fastmail/Outlook accounts and seeded messages.
//   - chromium only (not all three browsers) — minimal browser install, and the
//     tickets' behavior is browser-independent DOM logic. webkit would be
//     needed only for the 8n1v Safari user-activation nuance; that's called out
//     in the spec as a documented manual repro, not automated (the contract test
//     already pins the no-setTimeout code form).
//   - Skips if node/npx or the chromium browser is absent — see tests/e2e_test.rs.
//     The repo's contract tests (src/routes.rs) remain the always-on gate; this
//     suite is the end-to-end layer that runs when the toolchain is present.
//
// Run directly:  npx playwright test
// Run via cargo: cargo test --test e2e_test

const { defineConfig, devices } = require('@playwright/test');
const os = require('node:os');
const path = require('node:path');
const fs = require('node:fs');

// A temp config dir so the server boots with NO real accounts. The app's
// first-run path (no accounts) lands in settings; we then mock /api/accounts
// to inject fixture accounts before the app reads them.
//
// Lifecycle (the dir must not outlive the run in ANY end state):
//   - success/failure: tests/e2e/global-teardown.cjs removes the dir recorded
//     in SV_E2E_CONFIG_DIR (Playwright runs globalTeardown on both).
//   - hung/SIGKILLed run (teardown never fires): the next run's
//     sweepStaleConfigDirs() removes any sv-e2e-* dir older than 2 hours —
//     far longer than a healthy run of this fast sequential suite.
//   - worker processes: they re-evaluate this config but never launch the
//     webServer, so they must not mkdtemp at all (this used to leak one
//     empty dir per worker per run).
const CONFIG_DIR_STALE_MS = 2 * 60 * 60 * 1000;

function sweepStaleConfigDirs() {
  let entries = [];
  try {
    entries = fs.readdirSync(os.tmpdir());
  } catch {
    return;
  }
  for (const name of entries) {
    if (!name.startsWith('sv-e2e-')) continue;
    const dir = path.join(os.tmpdir(), name);
    try {
      // lstat, not stat: judge the entry's own age, and keep dangling
      // symlinks reapable (statSync would throw ENOENT on them forever).
      const st = fs.lstatSync(dir);
      if (Date.now() - st.mtimeMs > CONFIG_DIR_STALE_MS) {
        fs.rmSync(dir, { recursive: true, force: true });
      }
    } catch {
      // Another user's dir (EACCES) or removed mid-scan — never abort the run.
    }
  }
}

function tempConfigDir() {
  // Workers evaluate the config but never start the webServer; only the
  // runner needs (and should clean up) a real config dir.
  if (process.env.TEST_WORKER_INDEX !== undefined) return os.tmpdir();
  sweepStaleConfigDirs();
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'sv-e2e-'));
  fs.mkdirSync(path.join(dir, 'supervillain'), { recursive: true });
  process.env.SV_E2E_CONFIG_DIR = dir; // consumed by global-teardown.cjs
  return dir;
}

// The system under test: prefer the workspace build so specs assert against
// HEAD (cargo test guarantees target/debug exists); fall back to PATH only for
// direct `npx playwright test` runs on a machine without a workspace build.
// Always log the choice, and warn when the workspace build predates the newest
// src/ file — a direct npx run against a weeks-old target/debug would silently
// re-create the "specs assert the wrong binary" skew in the other direction.
function workspaceBinary() {
  const built = path.join(__dirname, 'target', 'debug', 'supervillain');
  if (!fs.existsSync(built)) {
    console.log('[e2e] SUT: `supervillain` from PATH (no workspace target/debug build)');
    return 'supervillain';
  }
  const builtAt = fs.statSync(built).mtimeMs;
  // The binary embeds the whole frontend at compile time (include_str! of
  // static/*.js etc.), so the scan must cover static/ — the dominant edit
  // type for what these specs assert — plus build.rs, and recurse: a nested
  // edit (src/platform/*.rs, static/mobile/*.js) doesn't bump its parent
  // directory's mtime (roborev 390).
  // throwIfNoEntry:false — this is a purely diagnostic check running at
  // config-load time; a dangling symlink or a file deleted mid-scan (editor
  // swap file) must at worst skew the warning, never abort the suite
  // (roborev 391). Cargo.toml/Cargo.lock included: a dependency bump also
  // leaves target/debug stale.
  let newestSrc = 0;
  for (const root of ['src', 'static', 'build.rs', 'Cargo.toml', 'Cargo.lock']) {
    const rootPath = path.join(__dirname, root);
    const rootSt = fs.statSync(rootPath, { throwIfNoEntry: false });
    if (!rootSt) continue;
    const entries = rootSt.isDirectory()
      ? fs.readdirSync(rootPath, { recursive: true }).map((f) => path.join(rootPath, f))
      : [rootPath];
    for (const f of entries) {
      const st = fs.statSync(f, { throwIfNoEntry: false });
      if (st && st.isFile() && st.mtimeMs > newestSrc) newestSrc = st.mtimeMs;
    }
  }
  console.log(`[e2e] SUT: ${built}`);
  if (newestSrc > builtAt) {
    console.warn('[e2e] WARNING: target/debug/supervillain is older than src/ — specs are asserting a stale build; run `cargo build` (or `cargo test`) first');
  }
  return built;
}

module.exports = defineConfig({
  testDir: './tests/e2e',
  globalTeardown: require.resolve('./tests/e2e/global-teardown.cjs'),
  fullyParallel: false, // one server at a time; specs are fast and sequential
  // Note: the app logs provider warnings on an empty config during boot;
  // that's expected and specs must not assert console silence.
  reporter: [['list']],
  use: {
    baseURL: 'http://127.0.0.1:8765',
    trace: 'retain-on-failure',
    headless: true,
    // The app renders times via toLocaleString(undefined, ...), i.e. the
    // browser locale, and the context locale defaults to the HOST system
    // locale — on a non-en machine the yq92 time-rendering assertions
    // ("10:00 AM", "CDT") would see 24-hour clocks and "GMT-5"-style zone
    // names. Pin the locale so the suite is machine-independent.
    locale: 'en-US',
  },
  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'] },
    },
  ],
  webServer: {
    // The freshly built workspace binary, against an empty temp config.
    // --no-browser so it doesn't try to open a browser. The server is the SUT —
    // its /api responses are mocked per-spec, but the shell
    // (index.html/app.js/style.css) it serves is the real thing under test.
    // target/debug is preferred so `cargo test --test e2e_test` (which builds
    // it) asserts specs against HEAD, not against whatever was last
    // `cargo install`ed — otherwise test outcomes track deploy state instead of
    // the working tree. PATH fallback only for direct `npx playwright test`
    // runs without a workspace build. Dedicated port 8765 so the suite never
    // collides with a running dev/deploy server on the default 8000.
    command: `${workspaceBinary()} --no-browser`,
    port: 8765,
    timeout: 30_000,
    reuseExistingServer: false,
    env: {
      // Scoped temp config so no real accounts/tokens load. Dedicated e2e port
      // so the suite is self-contained and doesn't disturb a running server.
      XDG_CONFIG_HOME: tempConfigDir(),
      SUPERVILLAIN_BIND: '127.0.0.1:8765',
      // Quieter logs during the run.
      RUST_LOG: 'warn',
    },
  },
});

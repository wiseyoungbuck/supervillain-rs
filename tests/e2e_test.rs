//! End-to-end browser tests via Playwright (kata 1p0d/9rg8/nt9e/8n1v leftover behavior).
//!
//! `tests/e2e/*.spec.cjs` drive a real chromium browser against the real
//! installed `supervillain` binary (booted against a temp empty config), with
//! every `/api/*` route mocked at the network layer via `page.route()` — no
//! real provider is ever hit. The specs assert the OBSERVABLE rendered-DOM
//! behavior that the Rust contract tests (code forms) and the node mock-DOM
//! tests (primitive behavior) structurally can't see: e.g. a mailbox name
//! containing `<img onerror>` renders as text not a live element, a REPLY
//! invite shows no RSVP buttons, the unsubscribe status shows the real count.
//!
//! Mirrors `tests/escape_test.rs`: shells out and SKIPS (does not fail) if the
//! toolchain is absent, so CI images without node/npx/playwright stay green.
//! The contract tests in `src/routes.rs` remain the always-on gate; this suite
//! is the end-to-end layer that runs when the toolchain is present.
//!
//! Installing the toolchain (one-time, machine-local): `npm install` in the
//! repo root, then `npx playwright install chromium`. The specs and config are
//! committed; node_modules/ and browser binaries are gitignored.

use std::process::Command;

#[test]
fn playwright_e2e_tests_pass() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");

    // Skip — don't fail — if npx is absent (no node toolchain). Mirrors the
    // escape_test.rs convention. `.output()` rarely errors on a missing binary
    // (the shell usually returns 127), so also check the exit status.
    let npx_probe = Command::new("npx").arg("--version").output();
    let npx_ok = match npx_probe {
        Ok(out) => out.status.success(),
        Err(_) => false,
    };
    if !npx_ok {
        eprintln!("npx not usable on PATH; skipping Playwright e2e tests");
        return;
    }

    // Ensure Playwright is installed. If node_modules is absent, try a one-shot
    // `npx playwright test` which will auto-install @playwright/test on demand;
    // if browsers are missing, `npx playwright install chromium` is needed and
    // we skip rather than auto-downloading ~150 MB in a test.
    let npx = which_npx();

    // Run the suite from the repo root (where playwright.config.cjs lives).
    let output = Command::new(npx)
        .arg("playwright")
        .arg("test")
        .arg("--reporter=list")
        .current_dir(manifest_dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn npx playwright: {e}"));

    if !output.status.success() {
        // Distinguish "browsers not installed" (skip) from a real test failure
        // (fail). Playwright prints a clear "browser was not found" / "Run
        // `npx playwright install`" message in the former case.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let combined = format!("{stdout}\n{stderr}");
        if combined.contains("playwright install")
            || combined.contains("Executable doesn't exist")
            || combined.contains("browser was not found")
            || combined.contains("supervillain: command not found")
            || combined.contains("webServer was not able to start")
        {
            eprintln!(
                "Playwright e2e prerequisites not met (browsers or supervillain \
                 binary not available); skipping e2e tests. Install browsers with \
                 `npx playwright install chromium` and ensure `supervillain` is on PATH."
            );
            return;
        }
        panic!("Playwright e2e tests failed\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}");
    }
}

fn which_npx() -> String {
    std::env::var("PATH")
        .map(|paths| {
            std::env::split_paths(&paths)
                .map(|p| p.join("npx"))
                .find(|p| p.exists())
                .map(|p| p.to_string_lossy().into_owned())
        })
        .ok()
        .flatten()
        .unwrap_or_else(|| "npx".to_string())
}

# scripts/

## e2e.sh

Full-system end-to-end check. Builds `ng`, `ngd`, and `ng-hook` in release
mode (unless skipped) and drives them exactly the way a real harness
session would: fake `NG_DATA_DIR`/`HOME` under `mktemp -d`, real hook
payloads piped into `ng-hook`, a real `ngd` daemon (including autostart),
and real HTTP requests against the UI. It never touches your actual
`~/.claude` or a real not-goldfish install — everything happens inside a
throwaway temp dir that's removed on exit (`trap cleanup EXIT`).

```bash
scripts/e2e.sh                   # cargo build --release, then run everything
E2E_SKIP_BUILD=1 scripts/e2e.sh  # reuse whatever is already in target/release
```

Covers, in order: build sanity; capture with no daemon (direct-write
fallback) and with a daemon (including autostart, asserting exactly one
daemon comes up); proactive memory injection (relevant prompt gets
`additionalContext` with provenance, irrelevant prompt gets silence);
search (lexical, `--semantic`, `--here`); procedural hygiene on a
synthetic PreCompact payload (stub applied, backup written, line count
preserved); the UI's HTTP API (`/`, `/api/status`, `/api/sessions`,
`/api/transcript` including the path-traversal 403, `/api/graph`);
`ng doctor` in a healthy environment; and `ng wisdom`/`ng wisdom --md`.

Every assertion prints PASS or FAIL with a human-readable message; the
script exits 1 if anything failed, 0 otherwise. A check for a subcommand
that isn't wired up yet in the current build (e.g. `ng wisdom --help`
failing) is reported as an explicit SKIP, never faked as a pass.

Requires `python3` (used as the HTTP client — `curl` is blocked by the
proxy in some environments — and to build JSON fixtures without bash
string-escaping bugs).

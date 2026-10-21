# NDDev OpenNetwork · gds

AGPL-3.0-only NDS adapter. [NDDev OpenNetwork](https://nddev.ai).

Read local Git status for an explicitly selected repository through the bundled
Rust worker. Canonical core owns the reader port and classification. The existing
status DTO remains available; partial reads carry no clean projection. Cached
ref comparisons always report unknown remote freshness. The reader performs no
fetch, Git mutation, credential access or network request.

Linux worker reads run under a 512 MiB address-space limit, three CPU seconds,
bounded scan/history/object/output budgets and the shared ProcessIo deadline and
cancellation owner. Windows/macOS calls return unsupported before process I/O
until native resource enforcement and acceptance are qualified. Portable DTOs
do not imply runtime support.

`contracts/module.json` is the canonical compiled-in runtime descriptor;
`module.yaml` identifies its repository package. Caller-owned local/process
capability handles share a bounded agent admission budget. Worker responses,
private paths and content are never Debug or telemetry payloads.

Run `just check` and `just provider-check`. Linux acceptance starts the actual
Rust worker against isolated Git fixtures and verifies dirty/index/ref
preservation, incomplete submodule reads, allocation bounds and cancellation.
There is no backup/recovery implementation or private estate configuration.

The opt-in `github-catalog` feature exposes one fixed authenticated
`github::read_user_and_page` Rust adapter for the session-owned NdsClient caller.
Default worker builds do not compile that HTTP client/SDK path. The caller lends
its protected repository credential inside Rust; NDS sign-in credentials,
ambient `gh` configuration and presentation tokens are not used. Core20 owns
ID validation, owner grouping and scoped continuation.

The adapter calls only GitHub `/user` and `/user/repos` with API `2026-03-10`,
page25, a1MiB decoded response cap and one10s deadline/cancellation lifetime.
It rejects redirects, uses no cookies, and returns generic structured errors.
Its mandatory async `revalidate` callback belongs to NdsClient: it checks the
actual Accounts authorization fence and selected Vault revision/pending ID
before each provider GET and before returning metadata, within that deadline.
There is no default production bypass; transport fixtures are not real-store
revocation acceptance.
Browser transport uses Rust/web_sys fetch with an owned AbortController and
does not change the Accounts same-origin transport or a browser cache. Browser
header copies remain within the accepted same-origin-script threat boundary;
zeroizing Rust buffers does not assert erasure of JavaScript engine memory.

`just catalog-check` runs controlled native HTTP negatives and deterministic
decoding rules. `just github-negative-check` uses a generated invalid credential
against fixed GitHub; it is denial evidence, not positive catalog acceptance.
Browser negatives are `wasm-bindgen-test` cases served with the owned
`scripts/check-github-browser.py` fixture. A positive provider credential fixture
has not been supplied. Credentials/fences, shared NdsClient and CSP are separate
owner integrations; the catalog profile performs no persistence or Git writes.

The separate opt-in `commit-sync` profile adds `commits::GitWorkspace` for an
existing checkout under an authorized `Developer/nddev/<owner>/<repository>`
root. Core owns portable locations and the commit cycle. The native adapter
holds one checkout lease, checks the exact origin/upstream and local witness,
requires a matching durable `.git/nds-binding.json` identity, fetches, and either
pushes an existing observed commit or fast-forwards a clean
worktree. Dirty, diverged, archived publication, partial and active-writer states remain
explicit refusals. It creates no commits, force updates, resets or rebases.

Git uses the caller's protected credential through a scoped HTTPS header outside
argv, with no ambient Git configuration, credential helper or terminal prompt.
Filters, included configuration, custom hooks and transport overrides are
refused. Required caller revalidation covers every command and completion;
cancellation, output and operation time are bounded by the existing ProcessIo.
Raw paths, refs, command output and credentials are not telemetry fields.

`just commit-check` exercises actual isolated Git publication, fast-forward,
preservation, checkout locking, cancellation and server-side hook refusal.
These checks establish local Git behavior; they do not establish positive
GitHub credentials or Mac/Windows runtime acceptance. Automatic discovery,
clone/relocation, durable selection, background scheduling and UI permission
integration remain separate A08 composition work. The default status-only
profile and its read-only descriptor remain unchanged.

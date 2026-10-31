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
`github::read_user_and_page` Rust adapter for the authorized Rust caller.
Default worker builds do not compile that HTTP client/SDK path. The caller lends
its protected repository credential inside Rust; NDS sign-in credentials,
ambient `gh` configuration and presentation tokens are not used. Canonical Core owns
ID validation, owner grouping and scoped continuation.

The adapter calls only GitHub `/user` and `/user/repos` with API `2026-03-10`,
page25, a1MiB decoded response cap and one10s deadline/cancellation lifetime.
It rejects redirects, uses no cookies, and returns generic structured errors.
Its mandatory async `revalidate` callback belongs to the original caller: it checks the
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
existing checkout under an authorized `Developer/NDS/<owner>/<repository>`
root. Core owns portable locations and the commit cycle. The native adapter
holds one checkout lease, checks the exact origin/upstream and local witness,
requires a matching durable `.git/nds-binding.json` identity, fetches, and either
pushes an existing observed commit or fast-forwards a clean
worktree. Dirty, diverged, archived publication, partial and active-writer states remain
explicit refusals. It creates no commits, force updates, resets or rebases.

Git uses the caller's protected credential through a scoped HTTPS header outside
argv, with no ambient Git configuration, credential helper or terminal prompt.
Unqualified submodules, filters, included configuration, custom hooks and transport overrides
are refused. Initialized unchanged-pin children follow the bounded profile below.
Required caller revalidation covers every command and completion;
cancellation, output and operation time are bounded by the existing ProcessIo.
Local Git inspections receive no credential and cannot start a network transport.
Raw paths, refs, command output and credentials are not telemetry fields.

Linux/macOS existing-checkout operations retain native handles for the checkout,
Git directory and local lock. They recheck those exact identities on both sides
of authorization awaits and before reporting completion. Replacing a directory
or deleting, replacing or symlinking its lock refuses the cycle; matching binding
bytes cannot transfer it to the replacement. These local witnesses use the same
native lock helpers as checkout connection.

The composition owner must retain and poll an admitted Git future through
cancellation until its actual result returns. A dropped waiter, timeout or local
refusal after push does not prove the remote effect was undone. The existing
result API does not provide a durable per-command receipt across process loss;
that integration remains with the kernel operation owner. Directory checks are
boundary observations, not an OS sandbox against other same-user native code.

`just commit-check` exercises actual isolated Git publication, fast-forward,
preservation, checkout locking, cancellation and server-side hook refusal.
These checks establish local Git behavior; they do not establish positive
GitHub credentials or Mac/Windows runtime acceptance. Relocation remains a separate explicit owner decision. The default status-only
profile and its read-only descriptor remain unchanged.

The explicit `GitHub commit acceptance` workflow runs only from this repository's
trusted `main` on a public hosted runner. It compiles the test before lending an
ephemeral, repository-scoped Actions credential, then exercises fixed-route
repository metadata, two real clones, bidirectional existing-commit publication,
fast-forward and dirty-work preservation. It owns one generated temporary branch
and confirms its conditional deletion; an unknown writer or creation outcome is
preserved for inspection. No personal token, extra repository or long-lived
credential is required. This qualifies Git provider access separately from the
user `/user` catalog and Accounts/Vault composition, which require their own
authorized fixtures. The ordinary test suite keeps this case opt-in.

The opt-in `catalog-runtime` feature is the shared catalog composition used by
GUI and CLI. It retains independent foreground and background cursors over the
same Core decisions, encrypted Vault state, original caller fences and provider
admission. Only the existing elected Vault replica invokes its collector after
catch-up; a follower does not start GitHub collection. Native window disposal
leaves that owner running. No additional scheduler or daemon is installed.

Enabled GDS requires this device's read-only, credential-store and local-network
consent and the saved owner-wide GitHub source. Every provider read and encrypted
publication rechecks that original source, consent and credential. A stale GUI
caller cannot borrow the retained owner's authorization. Catalog failures have
their own observation and never masquerade as a Vault replication failure.
Health is projected for its observed source and consent; each retained resource
keeps its original owner authority lifetime.

Complete traversals are checked every five minutes. Bounded parsed metadata and
ETags are retained only in memory for one immutable source, with authorization
rechecked even on `304`. Fixed-origin HTTP requests are serialized and honor
both `Retry-After` and primary quota reset. Missing secondary-limit headers cause
exponential pauses starting at one minute; six refusals end the retry episode
with an hour pause before a later probe. No credentials or response bodies enter
that cache or telemetry. `cargo test --features catalog-runtime,commit-sync` also
checks conditional reads, source isolation and actual transport refusal. These
controlled HTTP checks do not qualify positive GitHub access.

Explicit checkout connection is available to the shared Rust GUI/CLI client on
Linux and macOS with `catalog-runtime,commit-sync`. It requires the device's
separate `git_write` consent. Reading a catalog or enabling process execution
never implies that grant. The fixed GitHub metadata read confirms repository
and owner IDs before I/O and again before filesystem publication.

Connections use `~/Developer/NDS/<owner>/<repository>`. An existing checkout
is adopted only with a matching explicit GitHub origin; SSH aliases are not
resolved and its origin, index, branches and dirty files remain unchanged.
A new clone is prepared in owned staging and published with an OS no-replace
rename. Occupied paths, case collisions, symlinks, active writers and different
NDS/GitHub identities are refused. Git helpers share the existing bounded
ProcessIo owner; hooks, external object stores, replacement objects and automatic
maintenance cannot extend that scope.

If a checkout still occupies the previous `~/Developer/nddev/<owner>/<repository>`
location and the new target is absent, connection reports relocation required.
It preserves the old checkout and checks again before publishing a staged clone.
The module never moves existing work or creates a replacement automatically.
Existing initialized submodules are inspected separately with bounded native
commands after checking their paths, Git directories and configuration. Parent
commit cycles require unchanged gitlink paths, pinned commits and `.gitmodules`
between the local and fetched trees. Dirty or active children block the cycle;
their HEADs, indices and files are never updated. Traversal is bounded to 1,024
children, 16 levels and 8 MiB of observed output within the existing deadline.
Uninitialized or unsafe children and graph changes remain explicit refusals.
Recursive initialization, child publication and graph reconciliation are not
implemented by this profile.

Core owns the portable local marker and policy. Vault stores one encrypted
owner-wide connection intent per repository; device markers stay local rather
than multiplying replicated entities across 32 devices. A published checkout is
preserved if NDS registration must be retried. Relocation remains a separate explicit owner decision. Browser/mobile and Windows connection
are reported unsupported; portable DTOs are not claims of filesystem support.

With both profiles enabled, the elected native Vault owner also runs bounded
existing-commit cycles for these saved connections. One collection runs at a
time alongside Vault replication and authorization maintenance. It keeps the
same election lease until cancellation has drained Git; followers cannot start
another writer. Each attempt advances the connection traversal, including
refusals. Pages retain at most50 encrypted intents; a completed round waits five
minutes. No checkout is cloned by this traversal, and another device's intent
cannot substitute for this device's local binding or write consent.

Saved repository views expose this process's running/completed outcome and
completion time, separately from the saved provider observation. Source,
consent or owner replacement invalidates that progress; absence is no claim of
successful synchronization. Observations are bounded to1024 repositories and
are not additional replicated entities. Positive user-catalog access requires
its own authorized isolated user-account fixture.

Temporary proof-validation failures retain the exact original witness and
traversal progress. They admit no further Git operation or successful outcome;
saved views still validate that witness before exposing progress. Known scope
changes continue to discard it. Periodic effects also use that original opaque
read within its owning background catalog lifetime, revalidating it at every
provider, Git and completion boundary. They do not capture all repository
baselines again for each connection. Foreground connections keep independent
captures. `--guard-contention` checks both validation boundaries against a real
SQLite write lock, rejects borrowed or cancelled catalog witnesses, and verifies
that an actual disable clears the scope and re-enabling requires a new read.

A transient protected-read `Busy` receives one reread of the same original
proof inside the existing provider/Git deadline. It captures no new fence and
repeats no Git command or completed effect. A second refusal remains a refusal;
scope replacement, cancellation and all other errors retain their meaning.
The native contention proof also keeps the original checkout lease through a
real admission refusal, checks recovery within one Git attempt and rejects
cancellation during the reread. Same-session authority publication exercises
the original protected identity reader without another sign-in or device.

The opt-in Linux `scripts/periodic-fleet.py` accepts an owned Server
`--fixture-sync3` receipt, a new output directory and an explicit
`--work-root` outside this repository. The native execution/storage owner supplies
the existing admitted work root; the harness never falls back to HOME. Its
ephemeral repositories, replicas and credential stores use that root, and each
child receives its own TMPDIR there. Native provider temporary paths still need
target qualification. Use a path short enough for the native Unix sockets.
The path argument supplies placement, not storage or execution admission.
It drives 2–32 separate Rust processes, native Secret Services and replicas,
with 2–1024 generated repositories. It exercises explicit per-device write
consent, concurrent connection, ordinary publication/fast-forward, dirty-work
preservation, the unchanged five-minute round delay and a cold offline restart
with byte-identical encrypted pending operations. Start a fresh 14400-second
Server fixture after precompiling for the full run. Its generated loopback
repository metadata does not qualify GitHub discovery or provider rate limits;
the fixed-origin GitHub acceptance remains separate. Only the test executable
contains the fixture transport and workspace selectors. No installed runtime,
personal credential store or existing checkout is used.

Concurrent Vault canaries keep one 45-second admission/acknowledgement bound.
They retry only the pinned Vault's known pre-commit `Busy` refusal with the same
record ID and CAS; a successful mutation is never submitted again. Read-only
acknowledgement polling may also report `Busy`. Unknown writes, other errors and
deadlines fail the case. Write/read contention is recorded separately, and
`--guard-contention` requires actual owned SQLite pressure before a real canary
acknowledgement, alongside the original-proof checks above.

To prepare immutable member bytes separately from application execution, use
`python3 scripts/periodic-fleet.py prepare-member --output /absolute/new-artifact-directory`
on the assigned code-check host with clean committed source and its managed Cargo
profile. Deliver that directory through the existing native artifact route.
Pass `--member-manifest /absolute/artifact-directory/member.json` to the ordinary
fleet command on the admitted application host. This skips Cargo there and
requires the exact same clean Git commit/tree, lock, toolchain selection, features,
test name and copied executable SHA256. Before acquiring the Server fixture
lifetime it verifies the executable contains the exact ignored test using
`--list`; both process epochs then use those frozen bytes. An invalid artifact
leaves the Server fixture untouched. The manifest proves selected inputs and
integrity; execution authority, native ABI compatibility, resource/lease admission
and actual fleet outcomes remain the native owner's separate evidence.

Each round must produce a fresh terminal observation for every connection;
temporary admission/dependency refusals remain explicit in the receipt. Every
clean fixture repository must produce a successful in-scope result during the
run, and each round verifies the expected existing commit and dirty refusal.
Historical test coverage is separate from each repository's latest outcome.

Release journals live in [GitHub Releases](https://github.com/NDDev-Technocracy-OpenNetwork-NDS/nddev-device-sync-gds/releases).

# History/favorites execution evidence

## Objective and completion gates

Fully execute `docs/history-favorites-plan.md` on a new branch and push it. Completion requires retained current-slot history, favorites from queue/history/path/upload, front/back insertion, safe storage lifetime, migration, settings/UI/routes, documentation, security/regression tests, and an actual remote push. Passing the current tests alone does not establish completion.

## Work completed

Branch: `feature/history-favorites`.

- Baseline: `cargo test` via Nix toolchain: 91 passed, 0 failed, 1 ignored.
- CI command inspected: `cargo test --locked --all-targets --all-features`.
- Added descriptor v5 with defaulted immutable-picture map, ordered history records, and favorite records. Collections remain empty/inactive until lifetime management is implemented.
- Added composition identity based on source digest (derivative fallback), orientation, text recipe, and dedicated-text state. Excludes instance ID, display name, provenance, and rendering cache/profile metadata.
- Added v4 default/migration/round-trip and identity tests; corrected older migration tests to use the current descriptor version rather than a hardcoded v4.
- Verification: `cargo fmt --check`; CI test command: 93 passed, 0 failed, 1 ignored.

Toolchain: `cargo` is not on the default PATH. Run:

```sh
nix --extra-experimental-features 'nix-command flakes' shell \
  nixpkgs#cargo nixpkgs#rustc nixpkgs#rustfmt --command sh -c \
  'cargo fmt --check && cargo test --locked --all-targets --all-features'
```

## Audit: requirements not yet complete

The main plan's boxes intentionally remain unchecked until their whole requirement has evidence.

| Plan phase | Evidence / remaining work |
| --- | --- |
| 1 baseline/inspection | Baseline and key persistence/initialization paths inspected; exhaustive module/deletion audit still needed. |
| 2 persistent records | Schema/default migration and identity started; library operations, immutable object ownership, atomic reference behavior, and idempotency remain. |
| 3 storage lifetime | NOT implemented. Existing eager deletions and thumbnail overwrites are unsafe for retained records. Do not populate collections yet. |
| 4 history | NOT implemented: settings, centralized transition hook, seeding, trimming, tests. |
| 5 store operations | NOT implemented: favorite/remove/requeue operations and events. |
| 6 imports | NOT implemented: path/root security and favorites multipart upload. |
| 7 HTTP/UI | NOT implemented. |
| 8 verification | Only schema/identity and existing regressions covered; feature/security/concurrency/manual tests remain. |
| 9 docs/completion | User docs/examples, migration guidance, completed checklist, final audit remain. |

## Retention/store increment

- Implemented queue-to-favorite store operation with generation validation and idempotent duplicate adds; implemented favorite removal.
- Saved compositions own independent `saved-<opaque-id>` image/source/thumbnail files. This deliberately trades extra disk space for isolation from existing queue overwrite/delete behavior. Queue-owned files are never referenced by saved records; no queue deletion path needs to keep a saved-owned file alive.
- Saved-file GC follows descriptor reachability, runs under the mutation lock, and only removes the saved namespace. Removing the last favorite retains a snapshot if history still references it.
- Startup validates saved identity, asset namespace, references, and assets, and includes saved assets in orphan-cleanup reachability.
- Added tests for idempotency, queue orientation edits, queue removal, restart retention, final removal, and failed descriptor publication with snapshot cleanup.
- Verification before final namespace validation: CI test command: 95 passed, 0 failed, 1 ignored. Final verification is recorded in the commit/continuation report.
- History, UI, imports, saved previews, and requeue operations remain unimplemented. The objective is not complete.

## Next concrete action

Add current-slot capture consistently to ordinary commits and enqueue publication helpers; move snapshot copy I/O onto blocking workers before exposing HTTP handlers. Complete the deletion/publication audit and tests before enabling history/favorites. In particular inspect `preserve_legacy_source`, `remove`, `advance_internal`, `rerender_all`, orientation/text/edit operations, `publish_rerender`, `replace_derivative`, thumbnail publication, and startup orphan cleanup. Enqueues publish descriptors directly rather than exclusively through `commit_descriptor`, so a current-slot hook only in that method would miss initial/front enqueue transitions.

# Queue action concurrency

## Why consecutive actions used to fail

All queue forms submitted the descriptor generation captured when `/settings`
rendered. Removing a photo incremented that generation and triggered asynchronous
refill. If refill finished after the redirect rendered, every remaining form was
already stale. The next action returned a plain HTTP 409 until the user reloaded.
The generation also changes for unrelated edits and saved-library mutations.

## Server rules

CSRF validation remains mandatory. Checks and publication occur under the queue
mutation lock; refreshing the browser is not the concurrency guarantee.

| Action | Conflict rule |
| --- | --- |
| Remove | Remove the exact stable queue-entry ID from current state. An already removed ID is a successful no-op. |
| Reorder | Submit both the original displayed IDs and their proposed permutation. The original must still be an unchanged prefix of the current queue. Preserve newly appended entries at the end. Reject missing/duplicate/unknown proposed IDs, competing order changes, removals, front insertions, or advancement. |
| Advance | Submit the displayed order. Require it to remain an unchanged, nonempty prefix, allowing background appends but rejecting changes to the active item or displayed order. Never automatically retry. |
| Edit/orientation/text | Check a fingerprint of the displayed entry's metadata, recipe and render identity, both before rendering and under the publication lock. Preserve unrelated queue changes. Reject changes to, or removal of, this entry. |
| Favorite a queued photo | Check the displayed entry fingerprint so the saved composition is the one the user saw. Unrelated queue/library changes do not invalidate it. |
| Upload/message/insert saved picture | Insert new independent entries at the requested front/back position of the current queue. Do not overwrite existing entries or require a whole-queue generation. Rendering uses the settings-transition read lock. |
| Request supplier refresh/reset seen | Do not require the queue to remain at its displayed generation. |

Older reorder/advance/edit clients without the new baseline fields retain strict
whole-generation checks. Internal generation-checked store APIs remain available.
No descriptor migration is needed. Standalone library import/removal/history-save
forms and settings retain their existing checks; this change targets queue actions.

## Browser behavior

- Queue forms submit in-page and consume the server's redirected settings snapshot.
  Native forms still work without JavaScript.
- Every five seconds while the page is visible, fetch an uncached `/settings`
  snapshot. Replace queue cards, order baseline, and advancement controls together.
  A GET started before a mutation cannot overwrite its result.
- Never replace an unsaved reorder, an active drag, or an open editor during polling.
  Notify the user that newer state is available instead. Do not silently update a
  draft's concurrency token.
- Settings, message and file inputs are outside the refreshed region. Only the
  successfully submitted upload/message form is reset.
- Save or explicitly discard a draft order before another queue action. A genuine
  conflict preserves the draft, shows an in-page explanation, and offers **Review
  latest queue**. Entry-edit errors also appear inside the dialog.
- Disable submission while a queue request is outstanding. Never automatically
  replay a POST after a conflict, network failure, or authentication redirect.
- Native submit protection restores only buttons it disabled on back/forward
  navigation and removes its temporary submitter input. An empty queue's disabled
  advancement button remains disabled.

## Verification

```sh
cargo test
cargo build
# In another terminal: geckodriver --port 4444
node tools/verify-queue-concurrency-browser.mjs
node tools/verify-history-favorites-browser.mjs
```

Browser tools use only temporary fixture directories. `PAPERFRAME_BINARY`,
`WEBDRIVER_URL`, and `FIREFOX_BINARY` can override the defaults.

Route tests in `src/routes/tests/concurrency.rs` cover mixed sequences, refills
between render and submission, conflict responses, malformed reorder submissions,
CSRF, stale saved-picture additions, and per-entry edit protection. Queue-store
tests pause rendering to verify that publication merges an unrelated append but
rejects concurrent editing or deletion without resurrecting/overwriting entries.
The browser regression checks consecutive deletes, append-aware and competing
reorders, concurrent editing, preserved drafts, advancement replay protection,
empty/nonempty transitions, delegated card controls, and no document navigation.

# Initial Paper Frame Server Implementation Plan

## Goals

Build a small Rust HTTP server for trusted home-LAN use that:

- Serves a minimal settings/status webpage at `GET /settings`.
- Serves a configured JPEG at `GET /frame/current.jpg` for the offline e-ink frame.
- Records the UTC time, client IP, and response status of the most recent image `GET`.
- Supports HTTP modification checks so an unchanged image can return `304 Not Modified`.
- Is straightforward to configure, run, test, and extend later.

## Scope

Included:

- One process and one configured image slot.
- TOML configuration for the bind address and image path.
- In-memory last-request state, visible on the settings/status page.
- Structured request/application logging.
- `GET` and `HEAD` behavior for the image resource.
- Automated tests and basic operator documentation.

Not included:

- Image upload, transformation, DigiKam integration, scheduling, or multiple frames.
- Authentication, TLS, reverse-proxy support, or Internet-facing hardening.
- Persistent state/database storage.
- A settings-editing form or API.
- Proof that the physical display refreshed successfully; the server can only report the HTTP interaction.

## Immutable acceptance criteria

1. The project builds as a Rust binary and starts from a documented TOML configuration containing a socket bind address and image path.
2. `GET /settings` returns an HTML page with a Hello World-level interface and shows either that no image request has occurred or the latest recorded request's UTC timestamp, client IP, and response status.
3. With a readable configured JPEG, `GET /frame/current.jpg` returns its unchanged bytes with `200 OK`, `Content-Type: image/jpeg`, `Content-Length`, and `Last-Modified` headers.
4. An image `GET` with an applicable `If-Modified-Since` value returns `304 Not Modified` with no response body; both `200` and `304` image GETs update the last-request state.
5. The recorded client IP comes from the direct TCP peer address, not a caller-supplied forwarding header.
6. `HEAD /frame/current.jpg` returns the corresponding image headers and no body without replacing the last-GET state.
7. A missing configured image returns `404 Not Found`; an image read/metadata failure returns an appropriate server error. An attempted image GET still records its response status.
8. State is shared safely across concurrent requests and is intentionally reset when the process restarts.
9. Automated tests cover configuration parsing, both routes, image bytes and headers, conditional requests, peer-IP/state recording, HEAD behavior, and missing-image behavior.
10. Documentation explains startup, endpoint URLs, image replacement, cache behavior, state limitations, and the trusted-LAN-only security boundary.

## Design rules and constraints

- Use stable Rust with Axum/Tokio for HTTP/runtime functionality, Serde/TOML for configuration, `tracing` for logs, and a small HTTP-date library for standards-compliant dates.
- Keep application state in `Arc<AppState>` with a Tokio synchronization primitive; hold locks only while reading or replacing the small request record, never while doing file I/O.
- Represent the peer as `std::net::IpAddr`, discarding the ephemeral source port. Do not trust `X-Forwarded-For` or similar headers.
- Store timestamps in UTC and format HTTP dates according to HTTP requirements.
- Derive `Last-Modified` from image file metadata and compare `If-Modified-Since` at HTTP-date precision.
- Record what the server observed and attempted to return; do not describe a completed HTTP response as confirmation that the e-ink panel refreshed.
- Read the image from its configured path per request so replacing the file takes effect without restarting the server.
- Keep handlers and state independently testable. Avoid a database, frontend framework, template engine, and custom middleware unless evidence during implementation shows they are necessary.
- Fail startup with a clear error for missing or invalid configuration; runtime image failures must produce controlled HTTP responses rather than panic.
- Commit `Cargo.lock`, because this is an application.

## Implementation stages

### Stage 1 — Scaffold and configuration

- [x] Initialize the Rust binary crate and add the minimal runtime, HTTP, configuration, date, error-context, and tracing dependencies.
- [x] Add `.gitignore`, `config.example.toml`, and initial module boundaries for configuration, state, routes, and startup.
- [x] Define and validate configuration loading for `bind` and `image_path`, with useful startup errors.
- [x] Initialize structured logging and bind Axum using connect-info support so handlers can receive the direct peer address.
- [x] Add configuration parsing/validation tests.
- [x] Verify with formatting, linting, tests, and a startup attempt using the example configuration.
- [x] Commit the completed scaffold/configuration stage.

### Stage 2 — Shared state and settings page

Depends on Stage 1.

- [x] Define the shared application state and latest-image-request record (UTC timestamp, peer IP, and HTTP status).
- [x] Implement `GET /settings` as a small HTML response with a Hello World-level presentation and safe rendering of the current state.
- [x] Ensure the no-request state is explicit and concurrent reads/writes do not hold locks across I/O or response construction.
- [x] Test the settings page before and after injecting representative request state.
- [x] Verify formatting, linting, and all tests.
- [x] Commit the shared-state/settings-page stage.

### Stage 3 — Image endpoint and cache semantics

Depends on Stages 1–2.

- [x] Implement image metadata loading, JPEG byte serving, and `Content-Type`, `Content-Length`, and `Last-Modified` headers.
- [x] Implement `If-Modified-Since` comparison and bodyless `304 Not Modified` responses.
- [x] Implement explicit `HEAD` behavior with the same applicable headers and no body.
- [x] Update last-GET state for every image GET after its response status is known, including `200`, `304`, `404`, and server errors; do not update it for HEAD.
- [x] Map a missing image to `404` and other file/metadata failures to controlled server errors, with diagnostic logs and no panic.
- [x] Add tests using temporary image files and injected peer addresses for successful bytes/headers, conditional GET, state updates, HEAD, missing files, and route isolation.
- [x] Verify formatting, linting, and all tests.
- [x] Commit the image-serving/cache stage.

### Stage 4 — Operations documentation and robustness pass

Depends on Stages 1–3.

- [x] Add graceful shutdown for normal termination signals and concise startup/request logging.
- [x] Write a README covering prerequisites, configuration, startup, frame URL, `curl` examples, atomic image replacement guidance, cache behavior, and interpretation of last-request state.
- [x] Clearly document that state is in-memory, peer identity assumes a direct LAN connection, there is no authentication/TLS, and a successful GET does not prove panel refresh.
- [x] Review dependencies and code paths for unnecessary complexity, panics, lock misuse, accidental forwarding-header trust, and leaking filesystem details in HTTP errors.
- [x] Verify a release build in addition to formatting, linting, and tests.
- [x] Commit the documentation/robustness stage.

### Stage 5 — Final end-to-end validation

Depends on all previous stages.

- [ ] Start the server from a clean checkout with a temporary valid JPEG and documented configuration.
- [ ] Verify `/settings` initially reports no image request.
- [ ] Fetch `/frame/current.jpg` with `curl`; compare the response bytes to the source and inspect required headers.
- [ ] Verify `/settings` now reports the observed client IP, UTC time, and `200` status.
- [ ] Repeat the GET with `If-Modified-Since`; verify a bodyless `304` and updated status on `/settings`.
- [ ] Issue a HEAD request; verify headers/no body and confirm it did not replace the last-GET record.
- [ ] Replace the JPEG, verify the changed `Last-Modified` behavior and returned bytes, then remove it and verify `404` plus recorded status.
- [ ] Run `cargo fmt --check`, strict Clippy, the full test suite, and a release build; record the exact commands/results in the implementation summary.
- [ ] Commit any final validated fixes as a meaningful milestone, leaving the working tree clean.

## Execution contract

- Keep the plan updated as work progresses and check off completed items.
- Commit each completed stage or meaningful milestone.
- Implementation steps may be revised when new evidence warrants it.
- Design rules may be revised for a documented, well-supported reason.
- Goals and acceptance criteria must not be changed without explicit user approval.

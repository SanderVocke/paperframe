# OIDC authentication and split listener implementation plan

## Goals

- Integrate the server with Authelia, or another standards-compliant OpenID Connect provider, using the established Rust `openidconnect` protocol crate and server-side sessions.
- Make the HTTP exposure model explicit and configurable:
  - a LAN listener serves only the existing unauthenticated device endpoints;
  - a management listener requires authentication for settings, queue/supplier operations, private media, PWA assets, and a separate public-facing current-image endpoint;
  - minimal login/callback and health endpoints remain unauthenticated on the management listener.
- Preserve the current device-fetch and queue-advancement behavior on the LAN endpoint while making the authenticated image endpoint stable and non-advancing.
- Replace the process-global form CSRF token with an authenticated-session-bound token.
- Document secure bare-metal, reverse-proxy, container, and Kubernetes deployment, including the network controls needed to make the device listener LAN-only.

## Scope

### Included

- OIDC Authorization Code flow with PKCE S256, state, nonce, discovery, ID-token verification, login session creation/rotation, and local logout.
- In-memory server-side sessions, consistent with the documented single-replica deployment; restarting the process intentionally logs users out.
- Two independently bound Axum routers sharing the existing `AppState`.
- Browser-session authentication for the protected current-image endpoint.
- Configuration parsing, startup validation, route/authentication integration tests, examples, and deployment documentation.
- Workstation-complete verification using Rust tests, deterministic local OIDC fixtures, real loopback TCP listeners, formatting, clippy, tests, and a locked release build.

### Excluded

- Local usernames, passwords, password recovery, MFA, or account administration.
- TLS termination inside the Rust process; production HTTPS remains the responsibility of the reverse proxy/ingress.
- OAuth client-credentials, bearer-token introspection, static API keys, and other machine-to-machine authentication. These require separate acceptance criteria if the public authenticated image must be consumed by a non-browser client.
- Authorization roles or Authelia group restrictions; this change authorizes every identity successfully authenticated for the configured OIDC client.
- Multi-replica/shared session storage and OIDC provider-initiated or RP-initiated single logout. Logout clears the Paper Frame session; the Authelia SSO session may remain active.
- Claiming production readiness from mock-provider tests alone. Real Authelia, Linux container/runtime, reverse-proxy, DNS/TLS, firewall, and Kubernetes validation are retained below as mandatory future deployment work, not as workstation implementation blockers.

## Validation environment boundary

The implementation is being developed on a Windows/MSYS workstation with Rust/Cargo and OpenSSL. Docker/Podman, Authelia, Kubernetes tooling, Caddy/nginx, `cargo-audit`, and `cargo-deny` are not currently available. The implementation stages must therefore be fully checkable with native Rust tests and loopback sockets. Checks that inherently depend on the target Linux deployment are isolated in **Stage 8** and must remain unchecked until run there.

## Immutable workstation acceptance criteria

1. The LAN listener exposes `GET` and `HEAD` for `/frame/current.jpg` and `/frame/current.bmp`; management, authentication, PWA, queue, supplier, private-media, and health routes are absent from that router and return `404`.
2. The management listener exposes health and OIDC bootstrap routes without an application session. Every settings route, mutating route, queue/private-media route, PWA asset, and authenticated image route rejects an unauthenticated request by starting the OIDC login flow or returning an appropriate unauthenticated response.
3. `/frame/authenticated/current.jpg` and `/frame/authenticated/current.bmp` return the current configured representation only after authentication. They support conditional `GET`/`HEAD`, never advance/rotate/drop the queue, and do not masquerade as a device fetch.
4. Existing `/frame/current.*` semantics remain unchanged on the LAN listener, including peer-IP qualification, completed-download advancement, cache behavior, request recording, and serialization.
5. Authentication uses OIDC Authorization Code flow with PKCE S256 and fresh, unpredictable, short-lived, single-use state and nonce values. The callback validates state, nonce, signature, issuer, audience, and token time claims before creating an authenticated session.
6. OIDC discovery and token HTTP clients do not follow redirects, use bounded timeouts, and obtain metadata only from the explicitly configured issuer. Secrets, authorization codes, tokens, cookies, PKCE verifiers, state, and nonce are not logged.
7. Session cookies are opaque server-side session identifiers with `Secure`, `HttpOnly`, `SameSite=Lax`, a host-only scope, and bounded idle and absolute lifetimes. The session identifier is rotated after successful login and invalidated on logout. Plain-HTTP callback/public URLs are rejected except for an explicitly supported loopback development configuration.
8. Post-login redirects are restricted to known local protected paths; externally supplied absolute or protocol-relative return URLs cannot produce an open redirect. Failed and replayed callbacks do not authenticate a session.
9. Every state-changing application request requires both an authenticated session and that session's unpredictable CSRF token. A token copied from another session is rejected. OIDC state remains separate from form CSRF protection.
10. Bootstrap configuration requires a management bind address and valid OIDC settings, permits an optional distinct LAN bind address, obtains the client secret from a named environment variable, and fails startup with a clear error for missing/invalid security-critical configuration or colliding listeners.
11. The server starts and shuts down both configured listeners coherently: bind/setup failure prevents partial service startup, a termination signal drains both servers, and background scheduler/refill shutdown behavior remains correct.
12. Automated tests cover the route exposure matrix, successful login, wrong/missing/replayed state and nonce, invalid issuer/audience/expiry/signature, secure cookie attributes and rotation, session expiry/logout, cross-session CSRF rejection, safe return paths, unauthenticated mutation denial, and the differing advancement behavior of LAN and authenticated image endpoints.
13. README, example configuration, container files, and Kubernetes examples no longer claim that the application has no authentication; they describe Authelia client setup, callback URL, secret injection, reverse-proxy requirements, listener publication, and LAN firewall/network-policy boundaries.
14. `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-targets --all-features`, and `cargo build --release --locked` all pass on this workstation.
15. The complete browser OIDC behavior is exercised against a deterministic in-process or locally spawned mock provider over loopback, and both application routers are exercised through real loopback TCP listeners. These tests must not depend on Docker, a system-installed proxy, public DNS, or the real Authelia server.

## Future Linux deployment acceptance checks

These checks are required before exposing the feature in production, but they are explicitly **not** criteria for completing or committing the workstation implementation:

1. The release/container runs as the intended non-root user on the target Linux architecture and passes its container health check.
2. Login, callback, local logout, session expiry, callback replay rejection, and PWA/private-media loading work against the actual Authelia issuer and configured confidential client.
3. The actual HTTPS reverse proxy preserves the fixed public origin and callback URI, does not bypass management authentication, and does not allow spoofed forwarding headers to affect trust decisions.
4. The unauthenticated listener is reachable from the intended LAN/frame but unreachable from the public Internet and untrusted networks, as demonstrated from both sides of the production firewall/NAT or Kubernetes network boundary.
5. Production secret injection supplies the raw OIDC client secret without placing it in images, ConfigMaps, command lines, logs, or source control.
6. Docker/Podman and, where applicable, Kubernetes manifest validation and rollout/smoke tests pass.
7. `cargo audit` and/or `cargo deny` run in a suitable Linux/CI environment; findings are resolved or explicitly documented before release.

## Design rules and constraints

- Do not implement or store passwords. Authelia remains responsible for credentials, MFA, and identity policy.
- Use `openidconnect` directly for security-sensitive protocol work. Keep the Axum integration small and application-owned rather than depending on a low-adoption all-in-one OIDC middleware.
- Use `tower-sessions` with a memory store. Store OIDC transactions, authenticated identity, authentication timestamps, and form CSRF material server-side; never place provider tokens or claims in client-readable storage.
- Treat `(issuer, subject)` as the identity. Do not identify or authorize users by mutable email/display-name claims.
- Use explicit router composition as the authorization boundary. Do not scatter optional authentication checks through individual management handlers.
- Do not use `X-Forwarded-For`, `Forwarded`, `Host`, or `X-Forwarded-Proto` to decide trust, callback URLs, or LAN membership. Use the configured external base URL and the listener's direct peer address.
- A second bind address limits application route exposure but is not itself a complete LAN security boundary. Container port publication, host firewall rules, Kubernetes Services/network policies, and router port-forwarding must prevent public access to the LAN listener.
- The LAN and management binds must be distinct. When the LAN listener is omitted, the unauthenticated device route is disabled rather than added to the management router.
- Keep health responses minimal and unauthenticated so container orchestration can probe the management listener without an OIDC dependency.
- Preserve response bodies, media types, cache headers, streaming-completion advancement, and queue consistency behavior unless an acceptance criterion explicitly changes them.
- Use fixed local destinations after login by default. If the originally requested protected path is retained, parse and allowlist it as a path-and-query only, with a small size bound and no authority/scheme.
- Return generic authentication errors to clients and structured, secret-free diagnostic context to logs.
- Pin compatible dependency versions in `Cargo.lock`; inspect newly introduced transitive features and dependency duplication with Cargo on the workstation. Advisory/deny tooling unavailable here remains an explicit Stage 8 release check.

## Execution contract

- Keep the plan updated as work progresses and check off completed items.
- Commit each completed stage or meaningful milestone.
- Implementation steps may be revised when new evidence warrants it.
- Design rules may be revised for a documented, well-supported reason.
- Goals and acceptance criteria must not be changed without explicit user approval.
- Stage 8 items must remain visible and unchecked when the workstation implementation is complete; they do not block checking off Stages 1–7 or committing the implementation, but they do block claiming production deployment validation.

## Staged implementation

### Stage 1 — Configuration and dependency foundation

- [ ] Add compatible `openidconnect`, `tower-sessions`, HTTP-client, URL/secret-handling, and constant-time comparison dependencies with only required features.
- [ ] Replace the single bootstrap `bind` with `management_bind` and optional `lan_frame_bind`; define OIDC issuer, client ID, external base URL, client-secret environment-variable name, and bounded session lifetime settings.
- [ ] Validate URL schemes, exact callback derivation, listener uniqueness, timeout/lifetime bounds, and loopback-only HTTP development exceptions without including secret values in `Debug` or errors.
- [ ] Resolve the client secret at startup from the configured environment variable and fail closed when it is absent or empty.
- [ ] Update config unit tests for valid production/development forms and every security-relevant rejection. Decide and document whether the old `bind` key produces a targeted migration error or is accepted as an alias for `management_bind`.

**Verification**

- [ ] Run configuration tests, `cargo fmt --check`, and `cargo clippy --all-targets --all-features -- -D warnings`.
- [ ] Confirm malformed/missing OIDC configuration cannot start a management listener and test failure output contains no secret.
- [ ] Commit the configuration/dependency milestone.

### Stage 2 — OIDC client, transaction state, and sessions

- [ ] Add a focused authentication module that performs provider discovery from the configured issuer using a no-redirect, timeout-bounded HTTP client.
- [ ] Implement login initiation with fresh state, nonce, and PKCE S256 material stored as a short-lived, one-time server-side transaction and a validated local return destination.
- [ ] Implement callback exchange and verification of state, nonce, ID-token signature, issuer, audience, expiry/not-before, and subject before creating the local identity.
- [ ] Rotate the session identifier on login, record bounded idle/absolute authentication times, issue a session-bound CSRF token, and avoid retaining provider access/refresh/ID tokens after validation.
- [ ] Implement local logout that requires an authenticated, CSRF-protected request and deletes the local session.
- [ ] Configure the opaque session cookie with the required name, attributes, expiry, and development exception.
- [ ] Add isolated protocol tests using a local deterministic mock provider/JWKS for success and all callback/token failures in acceptance criteria 5–9.

**Verification**

- [ ] Demonstrate in tests that PKCE is sent, backend redirects are refused, callback material is single-use, the cookie changes at login, and no failed callback authenticates a protected request.
- [ ] Run authentication-module tests plus formatting and clippy.
- [ ] Commit the OIDC/session milestone.

### Stage 3 — Split routers and centralized authorization

- [ ] Refactor `src/routes.rs` into explicit shared handlers plus `management_router` and `lan_frame_router` constructors.
- [ ] Put only the two current-device aliases on the LAN router, retaining direct `ConnectInfo<SocketAddr>` handling and all current advancement/request-recording behavior.
- [ ] Divide the management router into public health/authentication routes and one centrally protected route subtree containing the root/settings UI, PWA assets, queue/supplier actions and media, and authenticated current-image aliases.
- [ ] Implement the authenticated image handler by reusing stable-current response construction without the completion/advancement stream and without recording it as LAN frame activity.
- [ ] Define consistent browser behavior for unauthenticated safe requests and state-changing requests; never execute or parse a protected mutation before authentication succeeds.
- [ ] Ensure tracing identifies the listener/endpoint class while omitting identity-provider secrets and sensitive query values.

**Verification**

- [ ] Add table-driven tests proving the complete route exposure/protection matrix on both routers.
- [ ] Test both image formats and methods under manual, scheduled, and every-fetch advancement, including successful full-body LAN advancement and authenticated non-advancement.
- [ ] Run all route tests and confirm existing image/cache/queue behavior remains covered.
- [ ] Commit the router/listener-boundary milestone.

### Stage 4 — Session-bound form CSRF and management integration

- [ ] Remove the process-global CSRF token from `AppState` and introduce helpers/extractors that obtain or create the authenticated session's CSRF token.
- [ ] Pass the session token into settings rendering and validate it in every URL-encoded and multipart mutation using constant-time comparison.
- [ ] Add a logout control to the authenticated settings UI using the same CSRF mechanism.
- [ ] Update route tests to establish authenticated sessions and use their own CSRF values rather than reaching into `AppState`.
- [ ] Test missing tokens, malformed tokens, tokens from another session, multipart requests, expired sessions, logout, and unauthenticated POSTs.

**Verification**

- [ ] Confirm every POST route in `src/routes.rs` appears in the authenticated router and invokes session-bound CSRF validation before mutation.
- [ ] Run all route/template tests, formatting, and clippy.
- [ ] Commit the CSRF/management milestone.

### Stage 5 — Multi-listener startup and shutdown

- [ ] Bind both configured sockets before serving either router so a bind failure cannot leave a partially exposed process.
- [ ] Initialize OIDC discovery/session state before announcing readiness, then serve both routers with direct connection information and shared application state.
- [ ] Coordinate listener failures, signal handling, graceful shutdown, and scheduler/refill termination; support the management-only case when `lan_frame_bind` is absent.
- [ ] Include listener purpose and address in startup logs without logging externally supplied credentials or OIDC transactions.
- [ ] Add startup-level tests where practical for two ephemeral listeners, omitted LAN listener, one-listener failure, and coordinated shutdown.

**Verification**

- [ ] Exercise both real TCP listeners in a test and verify management routes cannot be reached through the LAN socket and vice versa.
- [ ] Run signal/graceful-shutdown tests and the full Rust test suite.
- [ ] Commit the multi-listener runtime milestone.

### Stage 6 — Configuration examples and deployment assets

- [ ] Update `config.example.toml` and `docker/config.toml` with the listener and OIDC schema, environment-based secret reference, and safe explanatory comments.
- [ ] Update the Docker image to expose/document both ports while keeping health checks on the management listener; do not imply that `EXPOSE` makes the device port LAN-only.
- [ ] Update Kubernetes ConfigMap, Secret references, container ports, probes, and Services so management ingress and LAN device exposure can be controlled independently. Keep the default device exposure private and document platform-specific publication.
- [ ] Document an Authelia confidential OIDC client using Authorization Code flow, PKCE S256, the exact callback URI, and only required scopes.
- [ ] Update README and container guidance for browser login, local logout behavior, PWA requests, reverse-proxy HTTPS, trusted issuer/base URL configuration, firewall/network-policy requirements, upgrades from `bind`, and the new authenticated image URL.
- [ ] Explicitly document that automated public clients need a future bearer-token/client-credentials feature rather than URL credentials or attempts to automate browser OIDC.

**Verification**

- [ ] Parse every committed TOML example in Rust tests where values can be deterministic, and statically review Docker/Kubernetes changes for internally consistent ports, probes, environment names, and paths.
- [ ] Review docs to ensure no stale statement says the app lacks authentication or suggests publicly exposing the LAN listener.
- [ ] Record container build, Kubernetes schema/lint, and runtime smoke testing as pending Stage 8 work rather than representing those checks as completed locally.
- [ ] Commit the deployment/documentation milestone.

### Stage 7 — Workstation security and end-to-end validation

- [ ] Run a deterministic Rust mock OIDC provider/JWKS service and the application on two real loopback TCP listeners using the explicit loopback HTTP development exception. Do not require Docker, Authelia, public DNS, or a separately installed reverse proxy.
- [ ] Exercise the complete redirect/code flow with an HTTP client: unauthenticated navigation starts login, successful callback returns only to an allowlisted requested path, settings/PWA/private media work with the resulting session, local logout removes access, and callback replay fails.
- [ ] Verify direct LAN device `GET`/`HEAD`, conditional responses, full-download advancement, interrupted-download behavior, and peer-IP qualification through the LAN socket.
- [ ] Verify the authenticated image through the management socket requires a session, returns the same current bytes/media type, honors conditional requests, and never changes queue order or LAN frame activity.
- [ ] Attempt direct access to every management path through the LAN listener, spoofed forwarded headers, absolute/protocol-relative return URLs, cross-session CSRF reuse, wrong issuer/audience/nonce/state, expired tokens/sessions, and provider redirects.
- [ ] Assert the production cookie configuration includes `Secure`, `HttpOnly`, and `SameSite=Lax`, while separately testing the narrowly allowed loopback development mode.
- [ ] Capture and inspect normal and failure logs in tests or a local smoke run to ensure no secrets, codes, tokens, cookies, PKCE values, state, or nonce are emitted.
- [ ] Inspect `cargo tree` for unintended duplicate protocol/session stacks and unnecessary enabled features.
- [ ] Run `cargo fmt --check`.
- [ ] Run `cargo clippy --all-targets --all-features -- -D warnings`.
- [ ] Run `cargo test --all-targets --all-features`.
- [ ] Run `cargo build --release --locked`.
- [ ] Record evidence against every immutable workstation acceptance criterion, leave Stage 8 visibly pending, and commit the completed workstation implementation.

### Stage 8 — Future validation on the actual Linux server environment

> This stage cannot be completed honestly on the current workstation. Run it on the target Linux host or equivalent Linux CI/staging infrastructure before public exposure. Keep unchecked items in the plan as the deployment handoff checklist.

- [ ] Install or provide Docker/Podman and build the image from the committed lockfile; run it as the configured non-root UID with the production-like mounts and secret injection.
- [ ] Configure the real Authelia confidential OIDC client with the exact issuer, client ID, callback URI, Authorization Code grant, PKCE S256 requirement, and required scopes; inject the raw client secret through the server's secret mechanism.
- [ ] Put the management listener behind the actual HTTPS reverse proxy/ingress and confirm certificates, public origin, callback routing, request size/timeouts, WebSocket assumptions if any, and forwarded-header replacement behavior.
- [ ] Run real-browser login, MFA as configured by Authelia policy, callback, session expiry, local logout/re-login, PWA installation/update, private previews, mutations, and authenticated current-image checks.
- [ ] Confirm the real Authelia session behavior is accurately documented, especially that local logout may allow immediate SSO login while the Authelia session remains active.
- [ ] Publish the LAN listener only on the intended interface/port or private Kubernetes Service. Test reachability from the frame/LAN and non-reachability from an external or untrusted network; inspect firewall, NAT/port-forward, Service, Ingress/Gateway, and network-policy rules.
- [ ] Verify the management listener does not expose `/frame/current.*`, the LAN listener does not expose management/auth/health routes, and the public authenticated aliases cannot advance the queue.
- [ ] Validate Docker health checks and graceful termination with an in-flight frame response.
- [ ] If Kubernetes is used, run manifest schema/lint checks, apply to staging, inspect Secret/ConfigMap separation, verify probes and both Services, and perform rollout/rollback smoke tests while preserving the single-replica constraint.
- [ ] Install/run `cargo audit` and/or `cargo deny` against the release lockfile; resolve findings or record reviewed, time-bounded exceptions.
- [ ] Inspect production logs during successful and failed authentication for accidental disclosure of secrets or OIDC transaction material.
- [ ] Record command output, tested URLs, network vantage points, Authelia/server versions, and results for all future Linux deployment acceptance checks before declaring production validation complete.

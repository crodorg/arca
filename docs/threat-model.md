# arca — threat model (v1)

A short, opinionated read. The goal is not to chase every adversary; it is to
state which we plan against and which we explicitly do not.

## In scope

- **VPS compromise** (the email gateway used in Phase 7).
  - Mitigation: PGP-encrypted bodies; tokens never live on the VPS.
- **Casual LAN-level attacker** on the operator's home network.
  - Mitigation: RPC TCP listener bound to `127.0.0.1`, pf restricts to `wg0`.
- **Accidental credential leak in logs.**
  - Mitigation: secrets are decrypted into memory only; never logged at INFO or
    DEBUG; tracing fields explicit, not derive-default.
- **Plaid / Mercury / Stripe API token theft on the host.**
  - Mitigation: tokens live in `/etc/arca/secrets.age`, mode 0400, owned `_arca`.
    Decryption requires `secrets.key` (also 0400). Daemon reads both at start.

## Out of scope (v1)

- Nation-state adversary, persistent root kit on the OpenBSD host.
- Supply-chain attacks on Rust crates (mitigation: vendored crates + `cargo-vet`
  added in Phase 8 if there is community interest).
- Physical access to the router.
- Side-channel attacks on the SQLite file.

## Compensating practices (not enforced by code)

- Operator runs `signify` on monthly DB dumps. Off-host copy.
- WireGuard private key lives only on the two endpoints (router, laptop/Mac).
- `_arca` user has no shell (`/sbin/nologin`).
- The daemon `unveil`s only what it needs and `pledge`s before the event loop.

## What the daemon refuses

- Decrypting a malformed `secrets.age` and continuing without secrets — fail open
  is forbidden; instead start without secrets and log clearly.
- LLM calls in any code path.
- Any silent fallback for a provider error: errors are returned to the client,
  not papered over.

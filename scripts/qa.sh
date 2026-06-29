#!/bin/sh
# qa.sh — hardening / QA runner, layered on top of the `make check` gate.
#
# `make check` stays the canonical commit/push gate (fmt, clippy, test, file-size +
# debt caps, coverage ratchet). This script adds the heavier hardening tools on top:
# dependency audit, fuzzing, mutation testing, memory/UB checks, soak, and binary-size
# analysis. Run subcommands on demand; CI runs `qa.sh all` (the cheap, deterministic set).
#
# Quick start:
#   sh scripts/qa.sh install     # one-time, per machine
#   sh scripts/qa.sh all         # gate + audit (CI-safe)
#   sh scripts/qa.sh             # show this help
#
# Some subcommands need a nightly toolchain (rustup): fuzz, safety. They detect a
# missing toolchain and tell you what to install rather than failing cryptically.
#
# arca specifics: bare cargo fmt/clippy/test cover the whole workspace via cargo's
# resolver. The input-surface tools (fuzz, mutants, safety) scope to arca-core, the
# portable, deterministic, sanitizer-friendly crate — NEVER run them on the OpenBSD
# router (pledge/unveil is OS-enforced, not Rust-testable). The soak drives the real
# arca TUI binary against a throwaway daemon (temp socket + temp sqlite).

set -eu

# Run from the workspace root regardless of where invoked.
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

FUZZ_SECS="${FUZZ_SECS:-60}"      # per-target fuzz budget (qa.sh fuzz)
SOAK_SECS="${SOAK_SECS:-120}"     # soak duration (qa.sh stress)

have() { command -v "$1" >/dev/null 2>&1; }

# Require a tool or explain how to get it; returns non-zero (caller decides).
need() {
	if have "$1"; then return 0; fi
	echo "  ! missing: $1 — run: sh scripts/qa.sh install" >&2
	return 1
}

# Require rustup+nightly for the nightly-only tools; guide if absent.
need_nightly() {
	if ! have rustup; then
		echo "  ! no rustup on this box — '$1' needs a nightly toolchain." >&2
		echo "    install rustup, then: rustup toolchain install nightly" >&2
		return 1
	fi
	return 0
}

say() { printf '\n=== %s ===\n' "$1"; }

cmd_install() {
	say "install (global cargo tools)"
	# Stable-toolchain binaries.
	for t in cargo-nextest cargo-deny cargo-machete cargo-audit \
	         cargo-bloat cargo-llvm-lines cargo-mutants cargo-modules; do
		bin="${t#cargo-}"
		if have "$t" || cargo "$bin" --version >/dev/null 2>&1; then
			echo "  ok   $t"
		else
			echo "  --   installing $t"
			cargo install --locked "$t" || echo "  ! $t failed (skipping)"
		fi
	done
	echo
	echo "  Nightly-only tools (need rustup): cargo-fuzz, cargo-careful, miri, sanitizers."
	echo "  With rustup present:"
	echo "    rustup toolchain install nightly"
	echo "    rustup component add miri rust-src --toolchain nightly"
	echo "    cargo install --locked cargo-fuzz cargo-careful"
}

cmd_lint() {
	say "lint (fmt + clippy pedantic + machete)"
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	if need cargo-machete; then cargo machete; fi
}

cmd_test() {
	say "test (workspace)"
	if have cargo-nextest; then
		cargo nextest run
	else
		echo "  (cargo-nextest not installed; using cargo test)"
		cargo test
	fi
}

cmd_audit() {
	say "audit (cargo-audit + cargo-deny)"
	# Supply chain is de-prioritized per arca's threat model (out of scope for v1):
	# keep the cheap advisory scan + a deny.toml, record findings, don't chase fixes.
	if need cargo-audit; then cargo audit; fi
	if need cargo-deny; then
		[ -f deny.toml ] || { echo "  --   no deny.toml; running cargo deny init"; cargo deny init; }
		cargo deny check
	fi
}

cmd_fuzz() {
	say "fuzz (cargo-fuzz, ${FUZZ_SECS}s/target)"
	need_nightly fuzz || return 0
	need cargo-fuzz || return 0
	if [ ! -d fuzz ]; then
		echo "  ! no fuzz/ crate yet. arca-core is already a lib, so the targets reach it"
		echo "    directly. To scaffold: cargo fuzz init, then add targets for the untrusted"
		echo "    boundary — the RPC wire frame (rpc::decode_request/decode_response) and"
		echo "    provider JSON. See the hardening plan before adding these."
		return 0
	fi
	for t in $(cargo +nightly fuzz list 2>/dev/null); do
		echo "  -- fuzzing $t"
		cargo +nightly fuzz run "$t" -- -max_total_time="$FUZZ_SECS"
	done
}

cmd_mutants() {
	say "mutants (cargo-mutants — grades the test suite)"
	need cargo-mutants || return 0
	# Scope to arca-core's pure-logic surface: money (Cents invariants), pp (band
	# math), recurring (series detection). Daemon I/O + TUI render are not unit-graded.
	cargo mutants -p arca-core -f money.rs -f pp.rs -f recurring.rs
}

cmd_safety() {
	say "safety (miri + sanitizers + careful)"
	need_nightly safety || return 0
	host="$(rustc -vV | sed -n 's/host: //p')"
	# These prove the pure logic is UB-free. Two arca-specific scope limits:
	#  - arca-core has NO `unsafe` (all unsafe is OpenBSD FFI in the daemon —
	#    pledge/unveil/getpeereid — which is OS-enforced, not Linux-runnable), so the
	#    yield here is confirmation, not bug-finding.
	#  - Miri can't execute sqlite's bundled C (rusqlite FFI), so it is scoped to the
	#    FFI-free modules; the DB-backed tests run under `careful` below instead.
	# The daemon's tokio socket-serving concurrency is NOT exercised here — that is
	# the W4 soak's job.
	echo "  -- miri (FFI-free arca-core modules; sqlite C can't run under miri)"
	for m in money rpc time ids; do
		MIRIFLAGS="-Zmiri-disable-isolation" cargo +nightly miri test -p arca-core --lib "$m" \
			|| echo "  ! miri flagged $m (or needs: rustup component add miri rust-src)"
	done
	echo "  -- AddressSanitizer (arca-core lib, build-std; skip proptest)"
	ASAN_OPTIONS="detect_leaks=0:detect_odr_violation=0" RUSTFLAGS="-Zsanitizer=address" \
		cargo +nightly test -p arca-core --lib -Zbuild-std --target "$host" -- --skip prop \
		|| echo "  ! ASan run failed/flagged"
	echo "  -- ThreadSanitizer (arca-core lib, build-std; skip proptest)"
	RUSTFLAGS="-Zsanitizer=thread" \
		cargo +nightly test -p arca-core --lib -Zbuild-std --target "$host" -- --skip prop \
		|| echo "  ! TSan run failed/flagged"
	if have cargo-careful; then
		echo "  -- cargo-careful (arca-core)"; cargo +nightly careful test -p arca-core || echo "  ! careful flagged"
	fi
}

cmd_stress() {
	say "stress (PTY soak — drives the real TUI against a throwaway daemon, watches RSS)"
	# tests/soak.rs (arca-tui) spawns the release TUI in a pseudo-terminal, stands up
	# a throwaway daemon (temp socket + temp sqlite seeded with demo data), answers
	# the startup capability probe so init doesn't block in n_tty_read, forces the pty
	# raw so keystrokes are delivered, then drives navigation + resize/SIGWINCH for many
	# iterations while watching for panics, early exit, shutdown hangs, and RSS growth.
	# Ignored by the normal gate.
	cargo build --release
	if [ ! -f crates/arca-tui/tests/soak.rs ]; then
		echo "  ! no soak harness yet (crates/arca-tui/tests/soak.rs). See the hardening plan (W4)."
		return 0
	fi
	step="${ARCA_SOAK_STEP_MS:-15}"
	iters="${ARCA_SOAK_ITERS:-$(( SOAK_SECS * 1000 / step ))}"
	echo "  -- soak: ${iters} iters @ ${step}ms (~${SOAK_SECS}s) on target/release/arca"
	ARCA_SOAK_BIN="$ROOT/target/release/arca" \
	ARCA_SOAK_DAEMON="$ROOT/target/release/arca-daemon" \
	ARCA_SOAK_ITERS="$iters" \
	ARCA_SOAK_STEP_MS="$step" \
		cargo test --release -p arca-tui --test soak -- --ignored --nocapture
}

cmd_min() {
	say "min (bloat + llvm-lines + modules)"
	if need cargo-bloat; then cargo bloat --release --crates | head -25; fi
	if need cargo-llvm-lines; then cargo llvm-lines -p arca-core | head -25; fi
	if need cargo-modules; then
		cargo modules structure -p arca-core 2>/dev/null \
			|| echo "  ! cargo-modules CLI shape differs; run it manually"
	fi
}

cmd_gate() {
	say "gate (delegating to make check — the canonical project gate)"
	make check
}

cmd_all() {
	# CI-safe set: the project gate + dependency audit. Long/interactive tools
	# (fuzz, mutants, safety, stress) are run on demand, not here.
	cmd_gate
	cmd_audit
}

usage() {
	cat <<'EOF'
qa.sh — hardening / QA runner (layered on `make check`)

  install   cargo install the global tools (once per machine)
  lint      fmt --check + clippy pedantic + cargo-machete
  test      cargo-nextest run (falls back to cargo test)
  audit     cargo-audit + cargo-deny                  [security: supply chain]
  fuzz      run cargo-fuzz targets (FUZZ_SECS=60)     [security: input surface]
  mutants   cargo-mutants over arca-core — grade the test suite
  safety    miri + sanitizers + cargo-careful         [security: memory/UB]
  stress    PTY soak: drive the real TUI + RSS watch (SOAK_SECS=120) [stability]
  min       cargo-bloat + llvm-lines + modules
  gate      make check (the existing commit/push gate)
  all       CI-safe: gate + audit

  fuzz/safety need rustup+nightly; safety's sanitizers also need rust-src
  (rustup component add rust-src --toolchain nightly) for -Zbuild-std.
  fuzz + sanitizers run on a Linux/macOS dev box against arca-core — never the router.
EOF
}

case "${1:-help}" in
	install) cmd_install ;;
	lint)    cmd_lint ;;
	test)    cmd_test ;;
	audit)   cmd_audit ;;
	fuzz)    cmd_fuzz ;;
	mutants) cmd_mutants ;;
	safety)  cmd_safety ;;
	stress)  cmd_stress ;;
	min)     cmd_min ;;
	gate)    cmd_gate ;;
	all)     cmd_all ;;
	help|-h|--help) usage ;;
	*) echo "unknown subcommand: $1" >&2; usage; exit 2 ;;
esac

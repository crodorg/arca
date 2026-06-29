#!/usr/bin/env python3
"""arca-xmpp — XMPP bridge for arca. Runs on the OpenBSD router.

Outbound slixmpp client: dials ``arca@<domain>`` on the XMPP server over the
the mesh network interface. It does NOT listen on 5222 — the router only makes outbound
connections; inbound 5222 is blocked by pf. The operator messages ``arca@`` from
any device connected to the same XMPP server and gets deterministic answers.

Principle (the design spec "Hermes integration"): **arca owns truth, AI only
translates.** The deterministic allowlist below runs FIRST and needs neither
Hermes nor any LLM — if Hermes is ever removed, exact-syntax commands still work.
arca couples to the XMPP server, not to Hermes.

Natural-language translation is NOT arca's job: per the Hermes topology, Hermes
reaches ``arca@`` as a peer (the mesh network ACL ``tag:hermes -> tag:arca:5222``) and
sends already-translated exact commands, which hit the same allowlist below. arca
therefore never calls an LLM, which is the capability-scoping spine ("arca never
calls out"). The operator and Hermes are indistinguishable to arca — both just
send allowlisted verbs.

Two directions:
  * Pull (query): inbound message -> allowlist match -> arca-cli read verb ->
    reply. Read-only; uses read.sock.
  * Push (delivery): a periodic loop delivers what the daemon only *records*.
    The daemon never delivers (no proc/exec, no network out of the command
    path). This loop polls ``alert_history WHERE delivered=0``, pushes each to
    the operator's JID, and flips ``delivered=1`` with a direct local SQLite
    write; it also pushes new report/.ics files (deduped via a state file).

    Why a direct DB write and not a write-socket verb: this bridge is
    router-LOCAL and the flip is internal bookkeeping never driven by network
    input — so it widens no network/Hermes surface, and it keeps read.sock
    purely read-only (no mutation verb leaks onto the read path). The bridge
    opens arca.db read-write solely to run one parameterized
    ``UPDATE alert_history SET delivered=1 WHERE id=?``.

Config: /etc/arca/arca-xmpp.conf (ini); see bridge/arca-xmpp.conf.example.
Dependencies: slixmpp (pkg/pip). Everything else is stdlib.
"""

import asyncio
import configparser
import json
import logging
import re
import sqlite3
import subprocess
import sys
import time
from pathlib import Path

from slixmpp import ClientXMPP

CONF_PATH = "/etc/arca/arca-xmpp.conf"

# --- read-verb allowlist --------------------------------------------------
# Each entry maps a compiled regex over the (stripped, lowercased) message body
# to a builder that returns arca-cli args. WRITE verbs (refresh, manual.*) are
# absent by construction: the bridge speaks only to read.sock, which rejects
# them at dispatch anyway. The args are built here, never taken from the
# message, so no flag (e.g. --socket) can be injected by the sender.
SCOPES = ("month", "year", "ytd", "all")

ALLOWLIST = [
    (re.compile(r"^money$"), lambda m: ["money"]),
    (re.compile(r"^pp$"), lambda m: ["pp"]),
    (re.compile(r"^health$"), lambda m: ["health"]),
    (
        re.compile(r"^debt(?:\s+(%s))?$" % "|".join(SCOPES)),
        lambda m: ["debt", "--scope", m.group(1) or "month"],
    ),
    (
        re.compile(r"^tx(?:\s+([\w]+))?$"),
        lambda m: ["tx"] + (["--tag", m.group(1)] if m.group(1) else []),
    ),
    (re.compile(r"^business\s+([\w-]+)$"), lambda m: ["business", m.group(1)]),
    (
        re.compile(r"^alerts(?:\s+(all))?$"),
        lambda m: ["alerts"] + (["--all"] if m.group(1) else []),
    ),
]

HELP = (
    "commands: money | pp | health | debt [month|year|ytd|all] | tx [tag] | "
    "business <tag> | alerts [all]"
)


def match_verb(body):
    """Return arca-cli args for an exact allowlisted command, or None."""
    s = body.strip().lower()
    for rx, build in ALLOWLIST:
        m = rx.match(s)
        if m:
            return build(m)
    return None


def run_arca(args, cli, sock, timeout):
    """Run arca-cli against the read socket. Returns the reply text.

    --socket comes FIRST: it is a top-level flag, so it must precede the verb
    subcommand. Honest failure (the design spec): arca's own error is surfaced
    verbatim rather than masked.
    """
    cmd = [cli, "--socket", sock, *args]
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return f"arca-cli timed out after {timeout}s"
    except FileNotFoundError:
        return f"arca-cli not found at {cli}"
    if p.returncode != 0:
        return (p.stdout or p.stderr or "").strip() or f"arca-cli exit {p.returncode}"
    return p.stdout.strip() or "(empty response)"


# --- push side: deliver what the daemon only records ----------------------
# All DB/file helpers are blocking and run in an executor so the XMPP event
# loop is never stalled.


def _connect(db_path):
    con = sqlite3.connect(db_path, timeout=10)
    con.execute("PRAGMA busy_timeout = 10000")  # ride out the daemon's WAL writes
    return con


def fetch_undelivered_alerts(db_path):
    """Return [(id, message_text)] for alert_history rows with delivered=0.

    Each payload is rendered kind-aware (see _render_alert) — the rule kind comes
    from the joined alert_rules.rule_json, the same field the daemon validates.
    LEFT JOIN so an orphaned history row (rule since deleted) still delivers.
    """
    con = _connect(db_path)
    try:
        rows = con.execute(
            "SELECT ah.id, ah.fired_at, ah.payload_json, ar.rule_json "
            "FROM alert_history ah "
            "LEFT JOIN alert_rules ar ON ar.id = ah.rule_id "
            "WHERE ah.delivered = 0 ORDER BY ah.fired_at"
        ).fetchall()
    finally:
        con.close()
    out = []
    for aid, fired_at, payload, rule_json in rows:
        when = time.strftime("%Y-%m-%d %H:%M", time.localtime(fired_at))
        kind = None
        if rule_json:
            try:
                kind = json.loads(rule_json).get("kind")
            except (ValueError, TypeError):
                kind = None
        out.append((aid, f"⚠ arca alert [{when}]\n{_render_alert(kind, payload)}"))
    return out


def mark_alert_delivered(db_path, aid):
    """Flip one alert_history row to delivered. The only write the bridge makes."""
    con = _connect(db_path)
    try:
        con.execute("UPDATE alert_history SET delivered = 1 WHERE id = ?", (aid,))
        con.commit()
    finally:
        con.close()


def _load_pushed(state_file):
    try:
        return set(Path(state_file).read_text().split("\n")) - {""}
    except FileNotFoundError:
        return set()


def _record_pushed(state_file, key):
    with open(state_file, "a") as fh:
        fh.write(key + "\n")


def scan_new_files(cfg):
    """Return [(path_key, message_text)] for report/.ics files not yet pushed.

    First run (no state file) seeds the state with everything currently present
    and pushes nothing — so standing up the bridge against an existing
    reports_dir doesn't flood the operator with the whole back-catalogue.
    """
    state_file = cfg["state_file"]
    first_run = not Path(state_file).exists()
    pushed = _load_pushed(state_file)
    dirs = {d for d in (cfg["reports_dir"], cfg["ics_dir"]) if d}

    out = []
    for d in sorted(dirs):
        p = Path(d)
        if not p.is_dir():
            continue
        for f in sorted(p.iterdir()):
            if not f.is_file() or f.suffix not in (".md", ".ics"):
                continue
            key = str(f)
            if key in pushed:
                continue
            kind = "report" if f.suffix == ".md" else "calendar"
            try:
                body = f.read_text(errors="replace")
            except OSError as e:
                body = f"(could not read {f.name}: {e})"
            out.append((key, f"\U0001f4c4 arca {kind}: {f.name}\n\n{body}"))

    if first_run:
        for key, _ in out:
            _record_pushed(state_file, key)
        return []
    return out


# --- reply rendering ------------------------------------------------------
# arca-cli emits JSON tagged by "kind" (see arca-core rpc.rs `Response`). The
# TUI renders it; over XMPP a raw JSON blob is unreadable ("jibberish"). We
# render to plain prose HERE, at the presentation edge — arca-cli stays a pure
# deterministic JSON source of truth (no presentation in the money-truth
# layer). Money is `Cents` = a bare i64 of cents on the wire. Rules:
#   * non-JSON (arca's plain-text errors) passes through untouched — never mask
#     an honest failure;
#   * a known `kind` gets a hand-formatted layout;
#   * an unknown `kind` falls back to a generic key/value flattener, so a new
#     arca verb never regresses to a raw blob;
#   * the renderer never raises — on any error it degrades to the flattener.


def _ts(secs):
    try:
        return time.strftime("%Y-%m-%d %H:%M", time.localtime(int(secs)))
    except (TypeError, ValueError, OSError):
        return str(secs)


def _usd(cents):
    """Cents (i64) -> `$#,###.##` with sign in front of the `$`."""
    try:
        c = int(cents)
    except (TypeError, ValueError):
        return str(cents)
    a = abs(c)
    body = f"${a // 100:,}.{a % 100:02d}"
    return f"-{body}" if c < 0 else body


def _pct(x):
    try:
        return f"{float(x):.1f}%"
    except (TypeError, ValueError):
        return str(x)


def _dur(secs):
    try:
        s = int(secs)
    except (TypeError, ValueError):
        return str(secs)
    d, rem = divmod(s, 86400)
    h, rem = divmod(rem, 3600)
    m = rem // 60
    parts = []
    if d:
        parts.append(f"{d}d")
    if h:
        parts.append(f"{h}h")
    if m and not d:
        parts.append(f"{m}m")
    return " ".join(parts) or f"{s}s"


def _sub_amount(s):
    cur = (s.get("currency") or "USD").upper()
    v = s.get("latest", 0)
    if cur == "USD":
        return _usd(v)
    # CREDITS / MESSAGES: `latest` is a raw count, not cents (rpc.rs note).
    label = {"CREDITS": "credits", "MESSAGES": "messages"}.get(cur, cur.lower())
    try:
        return f"{int(v):,} {label}"
    except (TypeError, ValueError):
        return f"{v} {label}"


def _r_money(o):
    lines = [f"\U0001f4b0 net worth: {_usd(o.get('net_worth', 0))}"]
    if o.get("asof_secs"):
        lines.append(f"as of {_ts(o['asof_secs'])}")
    by_kind = o.get("by_kind") or []
    if by_kind:
        lines.append("by kind:")
        for r in by_kind:
            n = r.get("account_count", 0)
            lines.append(
                f"  • {r.get('kind', '?')}: {_usd(r.get('total', 0))}"
                f" ({n} acct{'' if n == 1 else 's'})"
            )
    subs = o.get("subscriptions") or []
    if subs:
        lines.append("subscriptions:")
        for s in subs:
            lines.append(f"  • {s.get('name', '?')}: {_sub_amount(s)}")
    return "\n".join(lines)


def _r_health(o):
    lines = [f"\U0001fa7a health: v{o.get('version', '?')}, up {_dur(o.get('uptime_secs', 0))}"]
    provs = o.get("providers") or []
    if not provs:
        lines.append("providers: (none)")
    else:
        lines.append("providers:")
        for p in provs:
            st = p.get("last_status") or "—"
            when = f", last poll {_ts(p['last_poll_at'])}" if p.get("last_poll_at") else ", never polled"
            lines.append(f"  • {p.get('label') or p.get('kind', '?')}: {st}{when}")
    return "\n".join(lines)


def _r_pp(o):
    lines = [
        f"\U0001f4ca permanent portfolio — T2 total {_usd(o.get('total', 0))}",
        f"band breach: {'YES ⚠' if o.get('band_breach') else 'no'}",
    ]
    rows = o.get("rows") or []
    if rows:
        lines.append("sleeves:")
        for r in rows:
            flag = " ⚠" if r.get("band_breach") else ""
            lines.append(
                f"  • {r.get('asset_class', '?')}: {_usd(r.get('actual_cents', 0))}"
                f" ({_pct(r.get('actual_pct', 0))} / target {_pct(r.get('target_pct', 0))},"
                f" drift {float(r.get('drift_pp', 0)):+.1f}pp){flag}"
            )
    bb = o.get("backbone") or {}
    if bb:
        lines.append(f"T1 backbone (hold-forever): {_usd(bb.get('total', 0))}")
        seg = [f"{k} {_usd(bb[k])}" for k in ("gold", "silver", "xmr", "land", "sfr", "other") if bb.get(k)]
        if seg:
            lines.append("  " + ", ".join(seg))
    return "\n".join(lines)


def _r_debt(o):
    lines = [f"\U0001f4b3 debt ({o.get('scope', 'month')}) — open {_usd(o.get('total_open', 0))}"]
    ob = o.get("open_balances") or []
    if ob:
        lines.append("balances:")
        for b in ob:
            lines.append(f"  • {b.get('account_name', '?')}: {_usd(b.get('balance', 0))}")
    sch = o.get("scheduled") or []
    if sch:
        lines.append("scheduled:")
        for s in sch:
            lines.append(f"  • {_ts(s.get('due_at'))} {_usd(s.get('amount', 0))} — {s.get('description', '')}")
    return "\n".join(lines)


def _r_business(o):
    return (
        f"\U0001f3e2 {o.get('display_name') or o.get('tag', '?')} ({o.get('scope', 'month')})\n"
        f"  income:   {_usd(o.get('income', 0))}\n"
        f"  expenses: {_usd(o.get('expenses', 0))}\n"
        f"  net:      {_usd(o.get('net', 0))}"
    )


def _r_tx(o):
    rows = o.get("rows") or []
    if not rows:
        return "\U0001f9fe no transactions"
    lines = [f"\U0001f9fe transactions ({len(rows)}):"]
    for r in rows:
        desc = r.get("description") or r.get("category") or ""
        tag = f" #{r['tag']}" if r.get("tag") else ""
        lines.append(
            f"  • {_ts(r.get('posted_at'))} {_usd(r.get('amount', 0))} "
            f"{r.get('account', '')} {desc}{tag}".rstrip()
        )
    return "\n".join(lines)


def _r_alerts(o):
    rows = o.get("rows") or []
    if not rows:
        return "\U0001f514 no pending alerts"
    lines = [f"\U0001f514 alerts ({len(rows)}):"]
    for r in rows:
        mark = "" if r.get("delivered") else " (new)"
        lines.append(f"  • [{_ts(r.get('fired_at'))}] {r.get('summary') or r.get('rule_name', '?')}{mark}")
    return "\n".join(lines)


def _generic(obj, indent=0):
    """Last-resort flattener: readable key/value text for any JSON shape."""
    pad = "  " * indent
    out = []
    if isinstance(obj, dict):
        for k, v in obj.items():
            if k == "kind":
                continue
            if isinstance(v, (dict, list)) and v:
                out.append(f"{pad}{k}:")
                out.append(_generic(v, indent + 1))
            else:
                out.append(f"{pad}{k}: {v}")
    elif isinstance(obj, list):
        for item in obj:
            if isinstance(item, (dict, list)):
                out.append(_generic(item, indent))
            else:
                out.append(f"{pad}• {item}")
    else:
        out.append(f"{pad}{obj}")
    return "\n".join(out)


def _render_alert(kind, payload):
    """Render one alert_history.payload_json (kind-aware) to prose — the push-side
    analogue of the daemon's summarize_alert. payload shapes (see alerts.rs):
      provider.stale  -> [{label, reason, ...}]
      balance.low     -> {account, balance_cents, min_cents}
      bandwidth.high  -> {account, used_gb, max_gb}
      pp.band_breach  -> [DriftRow {asset_class, actual_pct, band_breach, ...}]
      reminder        -> {message, day_of_month, hour_ast, minute_ast}
    Never raises: a parse error or unknown kind degrades to the generic
    flattener (or the raw string) so a new alert kind still delivers, unformatted
    rather than dropped.
    """
    try:
        obj = json.loads(payload)
    except (ValueError, TypeError):
        return payload
    try:
        if kind == "provider.stale" and isinstance(obj, list):
            if not obj:
                return "provider(s) stale"
            parts = [f"{p.get('label', '?')} ({p.get('reason', '?')})" for p in obj]
            return f"{len(obj)} provider(s) stale: " + ", ".join(parts)
        if kind == "balance.low" and isinstance(obj, dict):
            return (
                f"{obj.get('account', '?')} balance {_usd(obj.get('balance_cents', 0))}"
                f" below floor {_usd(obj.get('min_cents', 0))}"
            )
        if kind == "bandwidth.high" and isinstance(obj, dict):
            return (
                f"{obj.get('account', '?')} used {obj.get('used_gb', '?')} GB,"
                f" over {obj.get('max_gb', '?')} GB"
            )
        if kind == "pp.band_breach" and isinstance(obj, list):
            rows = [r for r in obj if r.get("band_breach")] or obj
            parts = [f"{r.get('asset_class', '?')} {_pct(r.get('actual_pct', 0))}" for r in rows]
            return f"{len(rows)} sleeve(s) out of band: " + ", ".join(parts)
        if kind == "reminder" and isinstance(obj, dict):
            return f"⏰ {obj.get('message', 'reminder')}"
    except Exception:  # noqa: BLE001 - never regress to a crash / dropped alert
        return payload
    return _generic(obj)  # known JSON, unknown kind: flatten, don't dump raw


_RENDERERS = {
    "money": _r_money,
    "health": _r_health,
    "pp": _r_pp,
    "debt": _r_debt,
    "business": _r_business,
    "tx_list": _r_tx,
    "alerts": _r_alerts,
}


def format_reply(raw):
    """Render arca-cli's JSON reply to plain prose. See the section comment."""
    s = (raw or "").strip()
    if not s:
        return "(empty response)"
    if s[0] not in "{[":
        return s  # plain text (e.g. an arca error string) — pass through
    try:
        obj = json.loads(s)
    except ValueError:
        return s
    if not isinstance(obj, dict):
        return _generic(obj)
    if obj.get("kind") == "error":
        return f"arca error [{obj.get('code', '?')}]: {obj.get('msg', '')}"
    fn = _RENDERERS.get(obj.get("kind"))
    try:
        return (fn(obj) if fn else _generic(obj)) or _generic(obj)
    except Exception:  # noqa: BLE001 - never regress to a raw blob / crash
        return _generic(obj)


class ArcaBridge(ClientXMPP):
    def __init__(self, cfg):
        super().__init__(cfg["jid"], cfg["password"])
        self.cfg = cfg
        self.auto_reconnect = True  # ride out the mesh network blips; push loop is gated on connection
        self._last = {}  # bare JID -> monotonic timestamp of last handled msg
        self._push_task = None
        self.register_plugin("xep_0030")  # service discovery
        # Active keepalive: ping the server every 60s and force a disconnect on a
        # missed pong, so a silently-dropped stream (hub/XMPP restart, the mesh network
        # re-key) is detected and `auto_reconnect` actually fires. Without enabling
        # it the plugin is inert and a half-dead socket goes unnoticed indefinitely.
        self.register_plugin("xep_0199", {"keepalive": True, "interval": 60, "timeout": 30})
        self.add_event_handler("session_start", self.on_start)
        self.add_event_handler("message", self.on_message)

    async def on_start(self, _event):
        self.send_presence()
        await self.get_roster()
        # Start the push loop once; session_start re-fires on reconnect, so
        # guard against spawning duplicates.
        if self.cfg["push_enabled"] and (self._push_task is None or self._push_task.done()):
            self._push_task = asyncio.create_task(self.push_loop())

    async def on_message(self, msg):
        if msg["type"] not in ("chat", "normal"):
            return
        sender = msg["from"].bare
        # Only the configured operator/Hermes JIDs may command arca; ignore the
        # rest silently (don't even acknowledge, to keep the surface quiet).
        if sender not in self.cfg["allowed_jids"]:
            return
        # Cheap per-sender rate guard against runaway loops.
        now = time.monotonic()
        if now - self._last.get(sender, 0.0) < self.cfg["min_interval"]:
            return
        self._last[sender] = now

        body = msg["body"] or ""
        args = match_verb(body)
        if args is None:
            msg.reply("couldn't parse. " + HELP).send()
            return
        # Run the blocking subprocess off the event loop so XMPP keepalives and
        # other messages aren't stalled.
        text = await asyncio.get_event_loop().run_in_executor(
            None, run_arca, args, self.cfg["arca_cli"], self.cfg["read_socket"], self.cfg["timeout"]
        )
        msg.reply(format_reply(text)).send()

    async def push_loop(self):
        """Deliver recorded alerts and new report/.ics files to the operator.

        Not cancel-safe across an in-flight send, but each item is marked
        delivered only AFTER its send is enqueued, so at worst a crash mid-loop
        re-pushes one item — never silently drops one.

        Honest-failure: the cycle is skipped entirely while disconnected, so an
        item is never marked delivered / recorded-pushed for a send that could
        not leave. Items stay queued and go out once slixmpp reconnects. This
        narrows the loss window from "the whole outage" to the rare race where
        the link drops mid-flush (the at-least-once edge noted above).
        """
        loop = asyncio.get_event_loop()
        to = self.cfg["operator_jid"]
        while True:
            await asyncio.sleep(self.cfg["poll_interval"])
            if not self.is_connected():
                continue
            try:
                alerts = await loop.run_in_executor(
                    None, fetch_undelivered_alerts, self.cfg["db_path"]
                )
                for aid, text in alerts:
                    self.send_message(mto=to, mbody=text, mtype="chat")
                    await loop.run_in_executor(
                        None, mark_alert_delivered, self.cfg["db_path"], aid
                    )
                files = await loop.run_in_executor(None, scan_new_files, self.cfg)
                for key, text in files:
                    self.send_message(mto=to, mbody=text, mtype="chat")
                    await loop.run_in_executor(
                        None, _record_pushed, self.cfg["state_file"], key
                    )
            except Exception as e:  # noqa: BLE001 - keep the loop alive, log loud
                print(f"arca-xmpp push: {e}", file=sys.stderr)


def load_cfg(path):
    cp = configparser.ConfigParser()
    if not cp.read(path):
        sys.exit(f"arca-xmpp: cannot read config {path}")
    x = cp["xmpp"]
    allowed = {j.strip() for j in x.get("allowed_jids", "").split(",") if j.strip()}
    if not allowed:
        sys.exit("arca-xmpp: allowed_jids is empty — refusing to answer everyone")
    return {
        "jid": x["jid"],
        "password": x["password"],
        "allowed_jids": allowed,
        "arca_cli": x.get("arca_cli", "/usr/local/bin/arca"),
        "read_socket": x.get("read_socket", "/var/run/arca/read.sock"),
        "timeout": x.getfloat("timeout_secs", 10.0),
        "min_interval": x.getfloat("min_interval_secs", 2.0),
        "host": x.get("host", fallback=None),
        "port": x.getint("port", 5222),
        # push side
        "push_enabled": x.getboolean("push_enabled", True),
        "operator_jid": x.get("operator_jid", fallback=next(iter(allowed))),
        "db_path": x.get("db_path", "/var/arca/arca.db"),
        "reports_dir": x.get("reports_dir", "/var/arca/reports"),
        "ics_dir": x.get("ics_dir", "/var/arca/reports"),
        "state_file": x.get("state_file", "/var/arca/arca-xmpp.state"),
        "poll_interval": x.getfloat("poll_interval_secs", 60.0),
    }


def main():
    # INFO-level trail to stderr; rc.d pipes it to syslog (rcctl set arca_xmpp
    # logger daemon.info). slixmpp logs connect/auth/bind/reconnect at INFO, so a
    # future stream drop leaves evidence instead of a silent multi-day outage.
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    cfg = load_cfg(CONF_PATH)
    xmpp = ArcaBridge(cfg)
    if cfg["host"]:
        xmpp.connect(address=(cfg["host"], cfg["port"]))
    else:
        xmpp.connect()
    xmpp.process(forever=True)


if __name__ == "__main__":
    main()

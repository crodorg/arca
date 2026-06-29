#!/usr/bin/env python3
"""Unit tests for the pure helpers in arca-xmpp.py.

Runnable without slixmpp (dev box / CI have no XMPP stack): we stub the slixmpp
import so the module loads, then exercise the deterministic functions that carry
the security-relevant logic — the verb allowlist (no flag injection from the
sender) and the file-push first-run seeding (no back-catalogue flood).

    python3 -m unittest bridge.test_arca_xmpp     # from repo root
    python3 bridge/test_arca_xmpp.py
"""

import importlib.util
import sys
import tempfile
import types
import unittest
from pathlib import Path

# Stub slixmpp before import: ArcaBridge subclasses ClientXMPP at class-definition
# time, so the import must resolve, but the pure helpers under test never touch it.
sys.modules.setdefault("slixmpp", types.SimpleNamespace(ClientXMPP=object))

_HERE = Path(__file__).resolve().parent
_SPEC = importlib.util.spec_from_file_location("arca_xmpp", _HERE / "arca-xmpp.py")
arca_xmpp = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(arca_xmpp)


class MatchVerbTests(unittest.TestCase):
    def test_known_verbs(self):
        self.assertEqual(arca_xmpp.match_verb("money"), ["money"])
        self.assertEqual(arca_xmpp.match_verb("PP"), ["pp"])  # case-folded
        self.assertEqual(arca_xmpp.match_verb("  health  "), ["health"])  # trimmed
        self.assertEqual(arca_xmpp.match_verb("debt"), ["debt", "--scope", "month"])
        self.assertEqual(arca_xmpp.match_verb("debt year"), ["debt", "--scope", "year"])
        self.assertEqual(arca_xmpp.match_verb("tx"), ["tx"])
        self.assertEqual(arca_xmpp.match_verb("tx income"), ["tx", "--tag", "income"])
        self.assertEqual(arca_xmpp.match_verb("business main"), ["business", "main"])
        self.assertEqual(arca_xmpp.match_verb("alerts"), ["alerts"])
        self.assertEqual(arca_xmpp.match_verb("alerts all"), ["alerts", "--all"])

    def test_rejects_unknown(self):
        for body in ("", "rm -rf /", "refresh", "alert-set foo", "moneys", "debt qtr"):
            self.assertIsNone(arca_xmpp.match_verb(body), body)

    def test_no_flag_or_command_injection(self):
        # Anchored regexes (^...$) mean trailing args never match, so the sender
        # cannot smuggle e.g. --socket to redirect onto the write socket, nor a
        # shell metacharacter (args go to subprocess as a list anyway).
        for body in (
            "money --socket /var/run/arca/write.sock",
            "health; reboot",
            "debt month && rm -rf /",
            "tx income | cat /etc/master.passwd",
            "business main --socket /x",
        ):
            self.assertIsNone(arca_xmpp.match_verb(body), body)


class ScanNewFilesTests(unittest.TestCase):
    @staticmethod
    def _cfg(d, state):
        return {"reports_dir": str(d), "ics_dir": str(d), "state_file": str(state)}

    def test_first_run_seeds_then_pushes_only_new(self):
        with tempfile.TemporaryDirectory() as td:
            d = Path(td)
            (d / "2026-04.md").write_text("old report")
            state = d / "state"
            cfg = self._cfg(d, state)

            # First run: existing file is recorded, nothing is pushed.
            self.assertEqual(arca_xmpp.scan_new_files(cfg), [])
            self.assertTrue(state.exists())
            self.assertIn("2026-04.md", state.read_text())

            # Steady state: the seeded file is not re-pushed.
            self.assertEqual(arca_xmpp.scan_new_files(cfg), [])

            # A genuinely new file IS returned (push_loop records it after send).
            (d / "arca-20260501.ics").write_text("BEGIN:VCALENDAR")
            out = arca_xmpp.scan_new_files(cfg)
            self.assertEqual(len(out), 1)
            key, msg = out[0]
            self.assertIn("arca-20260501.ics", key)
            self.assertIn("calendar", msg)

    def test_ignores_non_report_files(self):
        with tempfile.TemporaryDirectory() as td:
            d = Path(td)
            state = d / "state"
            state.write_text("")  # state exists -> not first run
            (d / "arca-xmpp.state").write_text("x")
            (d / "notes.txt").write_text("x")
            self.assertEqual(arca_xmpp.scan_new_files(self._cfg(d, state)), [])


if __name__ == "__main__":
    unittest.main()

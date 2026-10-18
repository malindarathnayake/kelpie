from __future__ import annotations

import copy
import unittest

from scripts import upstream_port as up

SHA_A = "a" * 40
SHA_B = "b" * 40
SHA_C = "c" * 40


def ledger(*commits: dict) -> dict:
    return {
        "upstream": {"url": "u", "branch": "master", "ref": "refs/x", "base": SHA_A},
        "commits": list(commits),
    }


class AuditTests(unittest.TestCase):
    def test_flags_herdr_names_in_added_lines_with_line_numbers(self) -> None:
        diff = (
            "+++ b/src/platform/macos/launchd_spawn.rs\n"
            "@@ -0,0 +20,3 @@\n"
            "+// Fine line.\n"
            '+const LABEL_PREFIX: &str = "dev.herdr.server.";\n'
            '+const DETACH_ENV: &str = "HERDR_LAUNCHD_DETACH";\n'
        )
        hits = up.audit_diff(diff)
        self.assertEqual([(h.path, h.line) for h in hits], [
            ("src/platform/macos/launchd_spawn.rs", 21),
            ("src/platform/macos/launchd_spawn.rs", 22),
        ])

    def test_ignores_removed_and_context_lines(self) -> None:
        diff = "+++ b/x.rs\n@@ -1,2 +1,2 @@\n herdr context\n-herdr removed\n+kelpie added\n"
        self.assertEqual(up.audit_diff(diff), [])

    def test_allows_upstream_credit_forms(self) -> None:
        for text in [
            "//! Ghostty died with every pane on Cmd-Q (herdr#4920).",
            "Ports herdr #5277.",
            "Ported from herdr #5174.",
            "See https://github.com/herdrdev/herdr/issues/4105 for the report.",
            "Kelpie is a fork of herdr.",
        ]:
            with self.subTest(text=text):
                self.assertFalse(up.offending(text))

    def test_credit_form_does_not_hide_a_real_name_on_the_same_line(self) -> None:
        self.assertTrue(up.offending("(herdr#4920) spawn herdr.exe"))

    def test_marker_keeps_a_deliberate_herdr_line(self) -> None:
        self.assertFalse(
            up.offending('let other = "herdr.sock"; // upstream-port: keep-herdr: detect herdr')
        )


class MessageTests(unittest.TestCase):
    def test_strips_pr_suffix_and_adds_port_note_and_trailer(self) -> None:
        message = up.compose_message("fix: enforce handshake deadlines (#5242)", "Why.", SHA_A)
        self.assertEqual(
            message,
            f"fix: enforce handshake deadlines\n\nWhy. Ports herdr #5242.\n\nUpstream-Commit: {SHA_A}\n",
        )

    def test_does_not_repeat_an_existing_port_note(self) -> None:
        message = up.compose_message("fix: x (#7)", "Why. Ports herdr #7.", SHA_A)
        self.assertEqual(message.count("herdr #7"), 1)

    def test_subject_override_keeps_the_upstream_pr(self) -> None:
        message = up.compose_message("fix: long upstream subject (#9)", "Why.", SHA_A, "fix: short")
        self.assertTrue(message.startswith("fix: short\n\nWhy. Ports herdr #9."))

    def test_rejects_empty_body_and_long_subject(self) -> None:
        with self.assertRaises(up.PortError):
            up.compose_message("fix: x (#1)", "  ", SHA_A)
        with self.assertRaises(up.PortError):
            up.compose_message("fix: " + "y" * 80, "Why.", SHA_A)


class LedgerTests(unittest.TestCase):
    def test_pending_keeps_upstream_order_and_drops_recorded(self) -> None:
        commits = [(SHA_A, "one"), (SHA_B, "two"), (SHA_C, "three")]
        recorded = ledger({"sha": SHA_B, "subject": "two", "status": "ported"})
        self.assertEqual(up.pending(commits, recorded), [(SHA_A, "one"), (SHA_C, "three")])

    def test_validation_rejects_bad_entries(self) -> None:
        cases = {
            "duplicate": ledger(
                {"sha": SHA_A, "subject": "s", "status": "ported"},
                {"sha": SHA_A, "subject": "s", "status": "ported"},
            ),
            "status": ledger({"sha": SHA_A, "subject": "s", "status": "maybe"}),
            "skip reason": ledger({"sha": SHA_A, "subject": "s", "status": "skipped"}),
            "short sha": ledger({"sha": "abc", "subject": "s", "status": "ported"}),
        }
        for name, bad in cases.items():
            with self.subTest(name):
                with self.assertRaises(up.PortError):
                    up.validate_ledger(bad)
        missing = ledger()
        del missing["upstream"]["base"]
        with self.assertRaises(up.PortError):
            up.validate_ledger(missing)

    def test_record_adds_pr_and_refuses_duplicates(self) -> None:
        data = ledger()
        entry = up.record(data, SHA_A, "fix: x (#12)", "skipped", "Not relevant.")
        self.assertEqual(entry["pr"], 12)
        with self.assertRaises(up.PortError):
            up.record(copy.deepcopy(data), SHA_A, "fix: x (#12)", "ported", None)

    def test_checked_in_ledger_is_valid(self) -> None:
        up.load_ledger()


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""In-process checks for the coding oracle fixture. No daemon."""
from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import coding_oracle  # noqa: E402


class CodingOracleTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.data = coding_oracle.load_fixture()

    def test_token_count_and_pack_stops_on_budget_boundary(self) -> None:
        self.assertEqual(coding_oracle.token_count(""), 0)
        self.assertEqual(coding_oracle.token_count("   "), 0)
        self.assertEqual(coding_oracle.token_count("a b  c"), 3)
        self.assertEqual(
            coding_oracle.pack(["aa bb", "cc", "dd ee"], 3),
            "aa bb\ncc",
        )
        self.assertEqual(coding_oracle.pack(["aa bb", "cc"], 3), "aa bb\ncc")
        self.assertEqual(coding_oracle.pack(["", "aa"], 1), "\naa")
        self.assertEqual(coding_oracle.pack(["aa bb cc dd"], 3), "")

    def test_fixture_shape(self) -> None:
        self.assertEqual(coding_oracle.invariant_failures(self.data), [])
        self.assertEqual(len(self.data["episodes"]), 12)
        self.assertEqual(self.data["needle_age_sec"], 3456000)
        for episode in self.data["episodes"]:
            self.assertEqual(len(episode["distractors"]), 4)

    def test_stuffing_pack_excludes_every_marker(self) -> None:
        budget = self.data["budget_tokens"]
        for episode in self.data["episodes"]:
            packed = coding_oracle.pack(coding_oracle.stuffing_chunks(episode), budget)
            self.assertNotIn(episode["marker"], packed, episode["id"])

    def test_each_needle_fits_in_the_budget_as_one_chunk(self) -> None:
        budget = self.data["budget_tokens"]
        for episode in self.data["episodes"]:
            content = episode["needle"]["content"]
            self.assertLessEqual(coding_oracle.token_count(content), budget, episode["id"])
            self.assertIn(episode["marker"], coding_oracle.pack([content], budget))

    def test_markers_unique_and_absent_from_queries_and_distractors(self) -> None:
        markers = [episode["marker"] for episode in self.data["episodes"]]
        self.assertEqual(len(markers), len(set(markers)))
        for episode in self.data["episodes"]:
            marker = episode["marker"]
            self.assertNotIn(marker, episode["query"])
            for distractor in episode["distractors"]:
                self.assertNotIn(marker, distractor["content"])

    def test_no_query_contains_colon(self) -> None:
        for episode in self.data["episodes"]:
            self.assertNotIn(":", episode["query"], episode["id"])

    def test_skill_actions_are_unique(self) -> None:
        actions: list[str] = []
        for episode in self.data["episodes"]:
            actions.append(episode["needle"]["skill_action"])
            for distractor in episode["distractors"]:
                actions.append(distractor["skill_action"])
        self.assertEqual(len(actions), len(set(actions)))


if __name__ == "__main__":
    unittest.main()

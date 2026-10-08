"""Provider-free checks for the async ReaderRuntime selection probe."""
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import async_reader_runtime_probe as probe


class RuntimeProbeTest(unittest.TestCase):
    def test_six_stages_and_worker_cue(self):
        stages = probe.inventory()
        self.assertEqual(len(stages), 6)
        self.assertEqual([len(stage["cards"]) for stage in stages], [3, 3, 4, 4, 4, 4])
        self.assertEqual(stages[0]["cue"].count("Current task: "), 1)
        self.assertNotIn("Recent task: ", stages[0]["cue"])
        prior = probe._fragment(stages[0]["cue"].removeprefix("Current task: "))
        self.assertTrue(stages[1]["cue"].startswith("Recent task: " + prior + "\nCurrent task: "))
        self.assertIn("Recent task: ", stages[-1]["cue"])
        self.assertTrue(all("host" not in card for stage in stages for card in stage["cards"]))

    def test_grade_retains_conflict_and_abstains_on_return(self):
        stages = probe.inventory()
        conflict = stages[4]["host"]
        self.assertTrue(probe.grade(conflict["keep"], conflict)["acceptable"])
        self.assertFalse(probe.grade(conflict["keep"][:1], conflict)["acceptable"])
        self.assertTrue(probe.grade([], stages[5]["host"])["acceptable"])
        self.assertFalse(probe.grade(stages[5]["host"]["drop"][:1], stages[5]["host"])["acceptable"])


if __name__ == "__main__":
    unittest.main()

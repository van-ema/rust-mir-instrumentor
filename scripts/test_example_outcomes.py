import unittest

from scripts.example_outcomes import (
    classify_expected,
    classify_observed,
    expectation_matches,
)


class ExampleOutcomeTests(unittest.TestCase):
    def test_observed_class_never_depends_on_expectation(self) -> None:
        self.assertEqual(classify_observed(None, False), "ok")
        self.assertEqual(classify_observed(None, True), "panic")
        self.assertEqual(classify_observed("TREE_BORROWS_VIOLATION|WRITE|RawMut|4", False), "violation")

    def test_ok_requires_clean_exit_without_violation(self) -> None:
        self.assertTrue(expectation_matches("ok", None, False))
        self.assertFalse(expectation_matches("ok", None, True))
        self.assertFalse(expectation_matches("ok", "WILD_POINTER|READ|RawConst|1", False))

    def test_panic_requires_abnormal_exit_without_violation(self) -> None:
        self.assertTrue(expectation_matches("panic", None, True))
        self.assertFalse(expectation_matches("panic", None, False))
        self.assertFalse(expectation_matches("panic", "OUT_OF_BOUNDS|READ|RawConst|1", True))

    def test_violation_requires_exact_signature(self) -> None:
        expected = "TREE_BORROWS_VIOLATION|WRITE|RawMut|4"
        self.assertTrue(expectation_matches(expected, expected, False))
        self.assertFalse(expectation_matches(expected, None, False))
        self.assertFalse(expectation_matches(expected, None, True))
        self.assertFalse(expectation_matches(expected, "TREE_BORROWS_VIOLATION|READ|RawMut|4", False))

    def test_expected_behavior_classes(self) -> None:
        self.assertEqual(classify_expected("ok"), "ok")
        self.assertEqual(classify_expected("pass"), "ok")
        self.assertEqual(classify_expected("panic"), "panic")
        self.assertEqual(classify_expected("TREE_BORROWS_VIOLATION|WRITE|RawMut|4"), "violation")


if __name__ == "__main__":
    unittest.main()

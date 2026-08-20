import unittest

from scripts.example_outcomes import (
    classify_expected,
    classify_observed,
    expectation_matches,
    semantic_agreement,
)
from scripts.run_miri_comparison import (
    derive_rusteze_class,
    discover_comparison_tests,
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

    def test_semantic_agreement_compares_pass_versus_reject(self) -> None:
        self.assertTrue(semantic_agreement("ok", "ok"))
        self.assertTrue(semantic_agreement("panic", "reject"))
        self.assertTrue(semantic_agreement("violation", "reject"))
        self.assertFalse(semantic_agreement("violation", "ok"))
        self.assertFalse(semantic_agreement("ok", "reject"))

    def test_comparison_does_not_infer_observation_from_expectation(self) -> None:
        row = {
            "expected": "TREE_BORROWS_VIOLATION|READ|RawConst|4",
            "observed": "-",
        }
        self.assertEqual(derive_rusteze_class(row, panicked=False), "ok")
        self.assertEqual(derive_rusteze_class(row, panicked=True), "panic")

    def test_comparison_discovery_includes_tree_variants(self) -> None:
        tests = discover_comparison_tests()
        labels = [test.comparison_label for test in tests]
        self.assertEqual(len(labels), len(set(labels)))
        self.assertIn("miri_sb_exact::pass_invalid_shr_tuple@tb_lite", labels)
        self.assertIn("tb_miri_micro::parent_read_kills_raw_child@tb_lite", labels)
        self.assertIn("sb_miri_micro::wrapper_reborrow_swap_ok@tb_lite", labels)
        self.assertIn("copy_alias_violation@tb_lite", labels)
        self.assertIn("ret_provenance_cases::strict_raw_add_oob_no_deref_ub@tb_lite", labels)


if __name__ == "__main__":
    unittest.main()

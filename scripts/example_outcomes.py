"""Shared classification helpers for example and Miri comparison runners."""

PASS_EXPECTATIONS = frozenset(("ok", "pass", "none"))
PANIC_EXPECTATIONS = frozenset(("panic", "panics"))


def classify_observed(signature: str | None, panicked: bool) -> str:
    """Classify what happened without consulting the expected outcome."""
    if signature:
        return "violation"
    if panicked:
        return "panic"
    return "ok"


def classify_expected(expected: str) -> str:
    """Reduce an expectation to pass-versus-reject behavior."""
    normalized = expected.strip().lower()
    if normalized in PASS_EXPECTATIONS:
        return "ok"
    if normalized in PANIC_EXPECTATIONS:
        return "panic"
    return "violation"


def expectation_matches(expected: str, signature: str | None, panicked: bool) -> bool:
    """Return whether the exact runner expectation matches the observed outcome."""
    normalized = expected.strip().lower()
    observed_class = classify_observed(signature, panicked)
    if normalized in PASS_EXPECTATIONS:
        return observed_class == "ok"
    if normalized in PANIC_EXPECTATIONS:
        return observed_class == "panic"
    return observed_class == "violation" and signature == expected

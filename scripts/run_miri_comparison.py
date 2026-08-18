#!/usr/bin/env python3
import csv
import os
import re
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path

try:
    from example_outcomes import (
        classify_expected,
        classify_observed,
        semantic_agreement,
    )
except ModuleNotFoundError:
    from scripts.example_outcomes import (
        classify_expected,
        classify_observed,
        semantic_agreement,
    )


REPO_ROOT = Path(__file__).resolve().parent.parent
EXAMPLES_DIR = REPO_ROOT / "examples"
REPORT_ROOT = REPO_ROOT / "reports" / "miri_compare"
PORT_RE = re.compile(r"^\s*//\s*Ported from (miri/tests/(?:fail|pass)/[A-Za-z0-9_./-]+\.rs)\.")
COMPILE_FLAGS_RE = re.compile(r"^\s*//@compile-flags:\s*(.*)$")
EXACT_PACKAGES = {
    "miri_sb_exact": (EXAMPLES_DIR / "miri_tests" / "sb_exact", "sb_lite", "stacked"),
    "miri_tb_exact": (EXAMPLES_DIR / "miri_tests" / "tb_exact", "tb_lite", "tree"),
    "miri_tb_pass_exact": (EXAMPLES_DIR / "miri_tests" / "tb_pass_exact", "tb_lite", "tree"),
    "miri_mem_exact": (EXAMPLES_DIR / "miri_tests" / "memory_exact", "tb_lite", "default"),
    "miri_function_calls_exact": (
        EXAMPLES_DIR / "miri_tests" / "function_calls_exact",
        "tb_lite",
        "tree",
    ),
    "miri_provenance_exact": (
        EXAMPLES_DIR / "miri_tests" / "provenance_exact",
        "tb_lite",
        "default",
    ),
    "miri_unaligned_exact": (
        EXAMPLES_DIR / "miri_tests" / "unaligned_exact",
        "tb_lite",
        "default",
    ),
}
TREE_MULTI_BIN_PACKAGES = {
    "tb_miri_micro": EXAMPLES_DIR / "tb_miri_micro",
    "sb_miri_micro": EXAMPLES_DIR / "sb_miri_micro",
    "ret_provenance_cases": EXAMPLES_DIR / "ret_provenance_cases",
}


@dataclass
class ComparisonTest:
    package: str
    bin_name: str
    src: Path
    origin: str
    rz_model: str
    miri_mode: str

    @property
    def label(self) -> str:
        if self.package == self.bin_name:
            return self.package
        return f"{self.package}::{self.bin_name}"

    @property
    def comparison_label(self) -> str:
        return f"{self.label}@{self.rz_model}"

    @property
    def report_name(self) -> str:
        return f"{self.package}__{self.bin_name}__{self.rz_model}"

    @property
    def run_log_dir_name(self) -> str:
        if self.package == self.bin_name:
            return self.package
        return f"{self.package}__{self.bin_name}"

    @property
    def miri_compile_flags(self) -> list[str]:
        flags: list[str] = []
        for line in self.src.read_text(errors="replace").splitlines():
            m = COMPILE_FLAGS_RE.match(line)
            if m:
                flags.extend(m.group(1).split())
        return flags


def die(msg: str) -> None:
    print(msg, file=sys.stderr)
    raise SystemExit(1)


def local_origin(src: Path) -> str:
    return str(src.relative_to(REPO_ROOT))


def discover_comparison_tests() -> list[ComparisonTest]:
    out: list[ComparisonTest] = []
    for package, (package_dir, rz_model, miri_mode) in EXACT_PACKAGES.items():
        for src in sorted((package_dir / "src" / "bin").glob("*.rs")):
            head = src.read_text(errors="replace").splitlines()[:3]
            for line in head:
                m = PORT_RE.match(line)
                if not m:
                    continue
                out.append(
                    ComparisonTest(
                        package=package,
                        bin_name=src.stem,
                        src=src,
                        origin=m.group(1),
                        rz_model=rz_model,
                        miri_mode=miri_mode,
                    )
                )
                if package == "miri_sb_exact":
                    out.append(
                        ComparisonTest(
                            package=package,
                            bin_name=src.stem,
                            src=src,
                            origin=m.group(1),
                            rz_model="tb_lite",
                            miri_mode="tree",
                        )
                    )
                break

    for package, package_dir in TREE_MULTI_BIN_PACKAGES.items():
        for src in sorted((package_dir / "src" / "bin").glob("*.rs")):
            out.append(
                ComparisonTest(
                    package=package,
                    bin_name=src.stem,
                    src=src,
                    origin=local_origin(src),
                    rz_model="tb_lite",
                    miri_mode="tree",
                )
            )

    standalone_dirs = sorted(
        package_dir
        for package_dir in EXAMPLES_DIR.iterdir()
        if package_dir.is_dir()
        and (
            package_dir.name.startswith("sb_lite_")
            or package_dir.name.startswith("tb_lite_")
            or package_dir.name == "copy_alias_violation"
        )
    )
    for package_dir in standalone_dirs:
        src = package_dir / "src" / "main.rs"
        if not src.is_file():
            continue
        out.append(
            ComparisonTest(
                package=package_dir.name,
                bin_name=package_dir.name,
                src=src,
                origin=local_origin(src),
                rz_model="tb_lite",
                miri_mode="tree",
            )
        )

    return sorted(out, key=lambda test: test.comparison_label)


def run(
    cmd: list[str],
    env: dict[str, str],
    cwd: Path,
    log: Path,
    timeout_s: float | None = None,
) -> int:
    with log.open("w") as f:
        f.write(f"$ {' '.join(cmd)}\n")
        f.flush()
        try:
            result = subprocess.run(
                cmd,
                cwd=cwd,
                env=env,
                stdout=f,
                stderr=subprocess.STDOUT,
                timeout=timeout_s,
            )
            return result.returncode
        except subprocess.TimeoutExpired:
            f.write(f"\nTIMEOUT after {timeout_s}s\n")
            return 124


def parse_summary(summary: Path) -> dict[str, dict[str, str]]:
    rows: dict[str, dict[str, str]] = {}
    with summary.open() as f:
        for row in csv.DictReader(f, delimiter="\t"):
            rows[row["example"]] = row
    return rows


def did_panic(log: Path | None) -> bool:
    if log is None:
        return False
    try:
        text = log.read_text(errors="replace")
    except OSError:
        return False
    return (
        "panicked at" in text
        or "thread 'main' panicked" in text
        or "thread caused non-unwinding panic. aborting." in text
        or "SIGABRT" in text
    )


def derive_rusteze_class(row: dict[str, str], panicked: bool) -> str:
    observed = row["observed"].strip()
    observed_panic = panicked or observed.lower() == "panic"
    signature = None if observed.lower() in ("", "-", "panic") else observed
    return classify_observed(signature, observed_panic)


def run_rusteze_test(test: ComparisonTest, report_dir: Path) -> tuple[Path, Path]:
    report_dir.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env["EXAMPLE_FILTER"] = rf"^{re.escape(test.label)}$"
    env["RZ_ALIAS_MODEL"] = test.rz_model
    env["REPORT_DIR"] = str(report_dir)
    env.setdefault("CARGO_INCREMENTAL", "0")
    if "-Zmiri-deterministic-concurrency" in test.miri_compile_flags:
        env["RZ_EXAMPLE_RUN_ATTEMPTS"] = "8"
        env["RZ_EXAMPLE_RUN_TIMEOUT_S"] = "10"
        env["RZ_EXAMPLE_KEEP_BEST_OBSERVED"] = "1"
    tool_dir = REPO_ROOT / "target" / "debug"
    local_cargo_tool = tool_dir / "cargo-instrument-mir"
    local_inst_tool = tool_dir / "instrument-mir"
    if local_cargo_tool.exists() and local_inst_tool.exists():
        env["PATH"] = f"{tool_dir}{os.pathsep}{env.get('PATH', '')}"
    cmd = [sys.executable, "scripts/run_example_tests.py"]
    log = report_dir / f"{test.report_name}.log"
    last_code = 0
    for _attempt in range(3):
        last_code = run(cmd, env, REPO_ROOT, log, timeout_s=60.0)
        summaries = sorted(report_dir.glob("*/summary.tsv"))
        if summaries:
            summary = summaries[-1]
            run_log = summary.parent / test.run_log_dir_name / "run.log"
            return summary, run_log
    die(f"rusteze example run failed for {test.label} ({test.rz_model}); see {log}")


def run_miri(test: ComparisonTest, report_dir: Path) -> tuple[int, str]:
    env = os.environ.copy()
    miri_flags = env.get("MIRIFLAGS", "").split()
    if test.miri_mode == "tree":
        miri_flags.append("-Zmiri-tree-borrows")
    miri_flags.extend(test.miri_compile_flags)
    env["MIRIFLAGS"] = " ".join(miri_flags).strip()
    cmd = ["cargo", "miri", "run", "-q", "-p", test.package, "--bin", test.bin_name]
    log = report_dir / f"{test.report_name}.miri.log"
    code = run(cmd, env, REPO_ROOT, log)
    text = log.read_text(errors="replace")
    if code == 0:
        return code, "ok"
    if "Undefined Behavior" in text or "ERROR" in text or "error:" in text:
        return code, "reject"
    return code, "other_fail"


def main() -> int:
    if shutil.which("cargo") is None:
        die("cargo not found")
    try:
        subprocess.run(
            ["cargo", "miri", "--version"],
            cwd=REPO_ROOT,
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    except Exception:
        die("cargo miri not available")

    tests = discover_comparison_tests()
    comparison_filter = os.environ.get("MIRI_COMPARE_FILTER", "").strip()
    if comparison_filter:
        tests = [
            test
            for test in tests
            if re.search(comparison_filter, test.comparison_label)
        ]
    if not tests:
        die(f"no Miri comparison tests found for filter {comparison_filter!r}")

    timestamp = time.strftime("%Y%m%d_%H%M%S")
    run_dir = REPORT_ROOT / timestamp
    rusteze_dir = run_dir / "rusteze"
    miri_dir = run_dir / "miri"
    rusteze_dir.mkdir(parents=True, exist_ok=True)
    miri_dir.mkdir(parents=True, exist_ok=True)

    rusteze_rows: dict[str, dict[str, str]] = {}
    rusteze_run_logs: dict[str, Path] = {}
    for test in tests:
        summary, run_log = run_rusteze_test(
            test,
            rusteze_dir / test.package / test.bin_name / test.rz_model,
        )
        row = parse_summary(summary).get(test.label)
        if row is None:
            die(f"missing rusteze row for {test.comparison_label}")
        rusteze_rows[test.comparison_label] = row
        rusteze_run_logs[test.comparison_label] = run_log

    summary_path = run_dir / "summary.tsv"
    with summary_path.open("w") as f:
        f.write(
            "label\torigin\trz_model\trusteze_status\trusteze_expected\t"
            "rusteze_observed\texpectation_class\trusteze_class\tmiri_mode\t"
            "miri_exit\tmiri_class\texpectation_agrees\tbehavior_agrees\n"
        )

    expectation_agree = 0
    behavior_agree = 0
    total = 0
    for test in tests:
        total += 1
        row = rusteze_rows.get(test.comparison_label)
        if row is None:
            die(f"missing rusteze row for {test.comparison_label}")
        rusteze_class = derive_rusteze_class(
            row,
            did_panic(rusteze_run_logs.get(test.comparison_label)),
        )
        expectation_class = classify_expected(row["expected"])
        miri_exit, miri_class = run_miri(test, miri_dir)
        expectation_same = semantic_agreement(expectation_class, miri_class)
        behavior_same = semantic_agreement(rusteze_class, miri_class)
        if expectation_same:
            expectation_agree += 1
        if behavior_same:
            behavior_agree += 1
        with summary_path.open("a") as f:
            f.write(
                "\t".join(
                    [
                        test.comparison_label,
                        test.origin,
                        test.rz_model,
                        row["status"],
                        row["expected"],
                        row["observed"],
                        expectation_class,
                        rusteze_class,
                        test.miri_mode,
                        str(miri_exit),
                        miri_class,
                        "yes" if expectation_same else "no",
                        "yes" if behavior_same else "no",
                    ]
                )
                + "\n"
            )

    print(f"comparison tests: {total}")
    print(
        f"expectation agreement: {expectation_agree}/{total} = "
        f"{expectation_agree / total:.1%}"
    )
    print(
        f"behavior agreement: {behavior_agree}/{total} = "
        f"{behavior_agree / total:.1%}"
    )
    print(f"summary: {summary_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

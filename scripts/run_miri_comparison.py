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


REPO_ROOT = Path(__file__).resolve().parent.parent
EXAMPLES_DIR = REPO_ROOT / "examples"
REPORT_ROOT = REPO_ROOT / "reports" / "miri_compare"
PORT_RE = re.compile(r"^\s*//\s*Ported from (miri/tests/fail/[A-Za-z0-9_./-]+\.rs)\.")
PACKAGE_DIRS = {
    "miri_sb_exact": EXAMPLES_DIR / "miri_tests" / "sb_exact",
    "miri_tb_exact": EXAMPLES_DIR / "miri_tests" / "tb_exact",
}


@dataclass
class PortedTest:
    package: str
    bin_name: str
    src: Path
    origin: str

    @property
    def label(self) -> str:
        return f"{self.package}::{self.bin_name}"

    @property
    def rz_model(self) -> str:
        return "sb_lite" if self.package == "miri_sb_exact" else "tb_lite"

    @property
    def miri_mode(self) -> str:
        return "stacked" if self.package == "miri_sb_exact" else "tree"


def die(msg: str) -> None:
    print(msg, file=sys.stderr)
    raise SystemExit(1)


def discover_ported_tests() -> list[PortedTest]:
    out: list[PortedTest] = []
    for package, package_dir in PACKAGE_DIRS.items():
        for src in sorted((package_dir / "src" / "bin").glob("*.rs")):
            head = src.read_text(errors="replace").splitlines()[:3]
            for line in head:
                m = PORT_RE.match(line)
                if not m:
                    continue
                out.append(
                    PortedTest(
                        package=package,
                        bin_name=src.stem,
                        src=src,
                        origin=m.group(1),
                    )
                )
                break
    return out


def run(cmd: list[str], env: dict[str, str], cwd: Path, log: Path) -> int:
    with log.open("w") as f:
        f.write(f"$ {' '.join(cmd)}\n")
        f.flush()
        result = subprocess.run(cmd, cwd=cwd, env=env, stdout=f, stderr=subprocess.STDOUT)
    return result.returncode


def parse_summary(summary: Path) -> dict[str, dict[str, str]]:
    rows: dict[str, dict[str, str]] = {}
    with summary.open() as f:
        for row in csv.DictReader(f, delimiter="\t"):
            rows[row["example"]] = row
    return rows


def derive_rusteze_class(row: dict[str, str]) -> str:
    expected = row["expected"].strip().lower()
    observed = row["observed"].strip().lower()
    if expected in ("ok", "pass", "none") and observed in ("", "-"):
        return "ok"
    if expected in ("panic", "panics") and observed == "panic":
        return "panic"
    if observed not in ("", "-"):
        return "violation"
    if expected not in ("ok", "pass", "none", "panic", "panics", "-", ""):
        return "violation"
    return "other"


def run_rusteze_test(test: PortedTest, report_dir: Path) -> Path:
    report_dir.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env["EXAMPLE_FILTER"] = rf"^{re.escape(test.label)}$"
    env["RZ_ALIAS_MODEL"] = test.rz_model
    env["REPORT_DIR"] = str(report_dir)
    env.setdefault("CARGO_INCREMENTAL", "0")
    cmd = [sys.executable, "scripts/run_example_tests.py"]
    log = report_dir / f"{test.package}__{test.bin_name}.{test.rz_model}.log"
    code = run(cmd, env, REPO_ROOT, log)
    if code != 0:
        die(f"rusteze example run failed for {test.label} ({test.rz_model}); see {log}")
    summaries = sorted(report_dir.glob("*/summary.tsv"))
    if not summaries:
        die(f"missing summary.tsv under {report_dir}")
    return summaries[-1]


def run_miri(test: PortedTest, report_dir: Path) -> tuple[int, str]:
    env = os.environ.copy()
    env.setdefault("MIRIFLAGS", "")
    if test.miri_mode == "tree":
        env["MIRIFLAGS"] = (env["MIRIFLAGS"] + " -Zmiri-tree-borrows").strip()
    cmd = ["cargo", "miri", "run", "-q", "-p", test.package, "--bin", test.bin_name]
    log = report_dir / f"{test.package}__{test.bin_name}.miri.log"
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

    tests = discover_ported_tests()
    if not tests:
        die("no exact Miri ports found")

    timestamp = time.strftime("%Y%m%d_%H%M%S")
    run_dir = REPORT_ROOT / timestamp
    rusteze_dir = run_dir / "rusteze"
    miri_dir = run_dir / "miri"
    rusteze_dir.mkdir(parents=True, exist_ok=True)
    miri_dir.mkdir(parents=True, exist_ok=True)

    rusteze_rows: dict[str, dict[str, str]] = {}
    for test in tests:
        summary = run_rusteze_test(test, rusteze_dir / test.package / test.bin_name)
        rusteze_rows.update(parse_summary(summary))

    summary_path = run_dir / "summary.tsv"
    with summary_path.open("w") as f:
        f.write(
            "label\torigin\trz_model\trusteze_status\trusteze_expected\trusteze_observed\trusteze_class\tmiri_mode\tmiri_exit\tmiri_class\tagree\n"
        )

    agree = 0
    total = 0
    for test in tests:
        total += 1
        row = rusteze_rows.get(test.label)
        if row is None:
            die(f"missing rusteze row for {test.label}")
        rusteze_class = derive_rusteze_class(row)
        miri_exit, miri_class = run_miri(test, miri_dir)
        same = (
            (miri_class == "ok" and rusteze_class == "ok")
            or (miri_class == "reject" and rusteze_class != "ok")
        )
        if same:
            agree += 1
        with summary_path.open("a") as f:
            f.write(
                "\t".join(
                    [
                        test.label,
                        test.origin,
                        test.rz_model,
                        row["status"],
                        row["expected"],
                        row["observed"],
                        rusteze_class,
                        test.miri_mode,
                        str(miri_exit),
                        miri_class,
                        "yes" if same else "no",
                    ]
                )
                + "\n"
            )

    print(f"ported tests: {total}")
    print(f"agreement: {agree}/{total} = {agree / total:.1%}")
    print(f"summary: {summary_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

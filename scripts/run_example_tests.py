#!/usr/bin/env python3
import json
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path


def die(msg: str) -> None:
    print(msg, file=sys.stderr)
    sys.exit(1)


def trim(s: str) -> str:
    return s.strip()


def extract_signature(log_path: Path) -> str | None:
    kind = None
    access = None
    size = None
    pkind = None
    in_block = False

    try:
        data = log_path.read_text(errors="replace").splitlines()
    except FileNotFoundError:
        return None

    for line in data:
        if "RUSTEZE VIOLATION" in line:
            in_block = True
            continue
        if not in_block:
            continue
        if kind is None:
            kind = trim(line)
            continue

        parts = line.split()
        if parts and parts[0] in ("READ", "WRITE"):
            access = parts[0]
            for part in parts:
                if part.startswith("size="):
                    size = part.split("=", 1)[1]
        if "kind=" in line and pkind is None:
            for part in parts:
                if part.startswith("kind="):
                    pkind = part.split("=", 1)[1]

        if line.lstrip().startswith("="):
            break

    if kind is None:
        return None
    if access is None:
        access = "UNKNOWN"
    if pkind is None:
        pkind = "unknown"
    if size is None:
        size = "unknown"
    return f"{kind}|{access}|{pkind}|{size}"


def did_panic(log_path: Path) -> bool:
    try:
        data = log_path.read_text(errors="replace")
    except FileNotFoundError:
        return False
    return "panicked at" in data or "thread 'main' panicked" in data


def normalize_alias_model(raw: str | None) -> str:
    if not raw:
        return "tb_lite"
    model = raw.strip().lower()
    if model in ("", "sb", "sb_lite", "stacked_borrows"):
        return "sb_lite"
    if model in ("tb", "tb_lite", "tree_borrows"):
        return "tb_lite"
    if model in ("none", "off"):
        return "none"
    return "tb_lite"


def read_expectation(pkg_dir: Path, bin_name: str, alias_model: str) -> str | None:
    candidate_files = [
        pkg_dir / f"expected.{bin_name}.{alias_model}.rz",
        pkg_dir / f"expected.{alias_model}.{bin_name}.rz",
        pkg_dir / f"expected.{alias_model}.rz",
        pkg_dir / f"expected.{bin_name}.rz",
        pkg_dir / "expected.rz",
    ]

    for expect_file in candidate_files:
        if not expect_file.exists():
            continue
        for line in expect_file.read_text(errors="replace").splitlines():
            line = trim(line)
            if not line or line.startswith("#"):
                continue
            return line

    src_file = pkg_dir / "src" / "main.rs"
    bin_src = pkg_dir / "src" / "bin" / f"{bin_name}.rs"
    if bin_src.exists():
        src_file = bin_src

    if src_file.exists():
        for line in src_file.read_text(errors="replace").splitlines():
            if "RZ_EXPECT:" in line:
                _, value = line.split("RZ_EXPECT:", 1)
                return trim(value)

    return None


def write_expectation(pkg_dir: Path, bin_name: str, alias_model: str, value: str) -> None:
    if alias_model == "tb_lite":
        expect_file = pkg_dir / f"expected.{bin_name}.rz"
    else:
        expect_file = pkg_dir / f"expected.{bin_name}.{alias_model}.rz"
    expect_file.write_text(f"{value}\n")


def source_file_for_example(pkg_dir: Path, bin_name: str) -> Path:
    bin_src = pkg_dir / "src" / "bin" / f"{bin_name}.rs"
    if bin_src.exists():
        return bin_src
    return pkg_dir / "src" / "main.rs"


def compile_flags_for_example(pkg_dir: Path, bin_name: str) -> list[str]:
    src_file = source_file_for_example(pkg_dir, bin_name)
    if not src_file.exists():
        return []
    flags: list[str] = []
    for line in src_file.read_text(errors="replace").splitlines():
        if not line.startswith("//@compile-flags:"):
            continue
        _, raw = line.split(":", 1)
        flags.extend(raw.strip().split())
    return flags


def load_examples(cargo: str, env: dict[str, str]) -> list[tuple[str, str, Path]]:
    output = subprocess.check_output(
        [cargo, "metadata", "--no-deps", "--format-version", "1"],
        text=True,
        env=env,
    )
    data = json.loads(output)
    examples: list[tuple[str, str, Path]] = []
    for pkg in data.get("packages", []):
        manifest_path = pkg.get("manifest_path", "")
        if "/examples/" not in manifest_path:
            continue
        pkg_dir = Path(manifest_path).parent
        for tgt in pkg.get("targets", []):
            if "bin" in tgt.get("kind", []):
                examples.append((pkg["name"], tgt["name"], pkg_dir))
    return sorted(examples)


def env_flag_enabled(env: dict[str, str], name: str) -> bool:
    raw = env.get(name)
    if raw is None:
        return False
    return raw != "0" and raw.lower() != "false"


def run_cmd(cmd: list[str], env: dict[str, str], log_path: Path) -> int:
    with log_path.open("a") as f:
        f.write(f"$ {' '.join(cmd)}\n")
        f.flush()
        result = subprocess.run(cmd, env=env, stdout=f, stderr=subprocess.STDOUT)
    return result.returncode


def instrument_example(
    cargo: str,
    base_env: dict[str, str],
    instr_cmd: list[str],
    repo_root: Path,
    run_dir: Path,
    log_dir_name: str,
    instr_log: Path,
) -> tuple[int, Path]:
    profile = "release" if "--release" in instr_cmd else "debug"

    if not env_flag_enabled(base_env, "RZ_INTERPROC_UNSAFE_SUMMARIES"):
        return run_cmd(instr_cmd, base_env, instr_log), repo_root / "target" / profile

    base_target_dir = run_dir / "_interproc_targets" / log_dir_name
    analyze_target_dir = base_target_dir / "summary-pass"
    final_target_dir = base_target_dir / "final"
    merged_summary_dir = base_target_dir / "merged-summaries"
    merge_report = merged_summary_dir / "merge.report.txt"

    shutil.rmtree(base_target_dir, ignore_errors=True)
    analyze_target_dir.mkdir(parents=True, exist_ok=True)
    merged_summary_dir.mkdir(parents=True, exist_ok=True)
    final_target_dir.mkdir(parents=True, exist_ok=True)

    analyze_env = base_env.copy()
    analyze_env["CARGO_TARGET_DIR"] = str(analyze_target_dir)
    analyze_env["RZ_ANALYZE_UNSAFE_SUMMARIES"] = "1"
    analyze_env["RZ_UNSAFE_DATAFLOW_SUMMARY_DUMP"] = "1"
    analyze_env["RZ_USE_UNSAFE_SUMMARIES"] = "0"

    if run_cmd(instr_cmd, analyze_env, instr_log) != 0:
        return 1, final_target_dir / profile

    summary_input_dir = analyze_target_dir / "rusteze-unsafe-summaries"
    if not summary_input_dir.is_dir():
        with instr_log.open("a") as f:
            f.write(f"missing summary dump dir: {summary_input_dir}\n")
        return 1, final_target_dir / profile

    merge_cmd = [
        sys.executable,
        str(repo_root / "scripts" / "merge_unsafe_summaries.py"),
        "--input-dir",
        str(summary_input_dir),
        "--output-dir",
        str(merged_summary_dir),
        "--report",
        str(merge_report),
    ]
    if run_cmd(merge_cmd, base_env, instr_log) != 0:
        return 1, final_target_dir / profile

    final_env = base_env.copy()
    final_env["CARGO_TARGET_DIR"] = str(final_target_dir)
    final_env["RZ_ANALYZE_UNSAFE_SUMMARIES"] = "0"
    final_env["RZ_UNSAFE_DATAFLOW_SUMMARY_DUMP"] = "0"
    final_env["RZ_USE_UNSAFE_SUMMARIES"] = "1"
    final_env["RZ_UNSAFE_SUMMARY_INPUT_DIR"] = str(merged_summary_dir)

    return run_cmd(instr_cmd, final_env, instr_log), final_target_dir / profile


def main() -> int:
    env = os.environ.copy()
    env.setdefault("CARGO_INCREMENTAL", "0")
    env.setdefault("RZ_LOG", "warn")
    env.setdefault("RZ_INSTRUMENT_ALL_DEPS", "1")
    env.setdefault("RZ_ALIAS_MODEL", "tb_lite")

    cargo = env.get("CARGO", "cargo")
    build_profile = env.get("BUILD_PROFILE", "debug")
    report_dir = Path(env.get("REPORT_DIR", "reports/example_tests"))
    allow_missing = env.get("ALLOW_MISSING_EXPECT", "0") != "0"
    record_expect = env.get("RECORD_EXPECT", "0") != "0"
    alias_model = normalize_alias_model(env.get("RZ_ALIAS_MODEL"))
    example_filter = env.get("EXAMPLE_FILTER", "").strip()

    if build_profile not in ("debug", "release"):
        die(f"Unknown BUILD_PROFILE={build_profile}. Use debug or release.")

    if shutil.which("cargo-instrument-mir", path=env.get("PATH")) is None:
        die("cargo-instrument-mir not found; run 'make tools' first.")
    if shutil.which("instrument-mir", path=env.get("PATH")) is None:
        die("instrument-mir not found; run 'make tools' first.")

    script_dir = Path(__file__).resolve().parent
    repo_root = script_dir.parent
    os.chdir(repo_root)

    profile_args: list[str] = []
    runtime_path = repo_root / "target" / "debug"
    bin_dir = repo_root / "target" / "debug"
    tool_dir = repo_root / "target" / "debug"
    if build_profile == "release":
        runtime_path = repo_root / "target" / "release"
        bin_dir = repo_root / "target" / "release"
        tool_dir = repo_root / "target" / "release"
        profile_args = ["--release"]

    # Prefer locally built tools if present, to avoid accidental mismatches between
    # the workspace state and any globally-installed `cargo-instrument-mir`.
    #
    # This is especially important for tests that exercise new instrumentation logic:
    # the example expectations track the repo's behavior, not the system install.
    local_cargo_tool = tool_dir / "cargo-instrument-mir"
    local_inst_tool = tool_dir / "instrument-mir"
    if local_cargo_tool.exists() and local_inst_tool.exists():
        env["PATH"] = f"{tool_dir}{os.pathsep}{env.get('PATH', '')}"

    build_cmd = [cargo, "build", "-p", "runtime"] + profile_args
    subprocess.run(build_cmd, env=env, check=True)

    timestamp = time.strftime("%Y%m%d_%H%M%S")
    run_dir = report_dir / timestamp
    run_dir.mkdir(parents=True, exist_ok=True)

    summary_file = run_dir / "summary.tsv"
    summary_file.write_text("example\tstatus\texpected\tobserved\n")

    examples = load_examples(cargo, env)
    if not examples:
        die("No example binaries found under examples/.")

    failures = 0
    missing = 0
    ran = 0

    for pkg_name, bin_name, pkg_dir in examples:
        label = pkg_name if bin_name == pkg_name else f"{pkg_name}::{bin_name}"
        if example_filter and not re.search(example_filter, label):
            continue

        ran += 1
        log_dir_name = pkg_name if bin_name == pkg_name else f"{pkg_name}__{bin_name}"
        log_dir = run_dir / log_dir_name
        log_dir.mkdir(parents=True, exist_ok=True)

        instr_log = log_dir / "instrument.log"
        run_log = log_dir / "run.log"
        mir_out = log_dir / f"out.{pkg_name}.mir"

        expected = read_expectation(pkg_dir, bin_name, alias_model)
        if expected is None and not allow_missing and not record_expect:
            with summary_file.open("a") as f:
                f.write(f"{label}\tmissing\t-\t-\n")
            print(
                f"missing expectation for {label} (model={alias_model}; add expected*.rz or RZ_EXPECT comment)",
                file=sys.stderr,
            )
            missing += 1
            continue

        instr_cmd = [
            cargo,
            "instrument-mir",
            f"--runtime-path={runtime_path}",
            f"--mir-out={mir_out}",
            "-p",
            pkg_name,
            "--bin",
            bin_name,
        ] + profile_args

        instr_log.write_text("")
        result_code, bin_dir_for_run = instrument_example(
            cargo,
            env,
            instr_cmd,
            repo_root,
            run_dir,
            log_dir_name,
            instr_log,
        )
        if result_code != 0:
            with summary_file.open("a") as f:
                f.write(f"{label}\tinstrument_fail\t{expected or 'missing'}\t-\n")
            failures += 1
            continue

        compile_flags = compile_flags_for_example(pkg_dir, bin_name)
        deterministic_concurrency = "-Zmiri-deterministic-concurrency" in compile_flags
        run_attempts = 8 if deterministic_concurrency else 1
        run_timeout_s = 10.0 if deterministic_concurrency else None
        bin_path = bin_dir_for_run / bin_name
        observed = None
        panicked = False
        for _attempt in range(run_attempts):
            with run_log.open("w") as f:
                try:
                    run_result = subprocess.run(
                        [str(bin_path)],
                        env=env,
                        stdout=f,
                        stderr=subprocess.STDOUT,
                        timeout=run_timeout_s,
                    )
                    timed_out = False
                except subprocess.TimeoutExpired:
                    f.write(f"\nTIMEOUT after {run_timeout_s}s\n")
                    class TimeoutResult:
                        returncode = 124
                    run_result = TimeoutResult()
                    timed_out = True

            observed = extract_signature(run_log)
            panicked = did_panic(run_log) or run_result.returncode != 0 or timed_out
            expected_lower = (expected or "").lower()
            matched = False
            if expected_lower in ("ok", "pass", "none"):
                matched = observed is None and not panicked
            elif expected_lower in ("panic", "panics"):
                matched = panicked and observed is None
            elif expected is not None:
                matched = observed == expected
            if matched:
                break

        if expected is None and record_expect:
            expected = observed or "ok"
            write_expectation(pkg_dir, bin_name, alias_model, expected)
        elif record_expect and alias_model != "tb_lite":
            # For non-default models, allow recording only the behavioral deltas:
            # if observed differs from the default expectation, materialize a
            # model-specific expected.<bin>.<model>.rz file.
            model_observed = observed or ("panic" if panicked else "ok")
            if expected != model_observed:
                write_expectation(pkg_dir, bin_name, alias_model, model_observed)
                expected = model_observed

        if expected is None:
            # Missing expectations are allowed; record what we saw and move on.
            with summary_file.open("a") as f:
                f.write(f"{label}\tmissing\t-\t{observed or '-'}\n")
            continue

        expected_lower = expected.lower()
        if expected_lower in ("ok", "pass", "none"):
            if observed is None:
                with summary_file.open("a") as f:
                    f.write(f"{label}\tok\t{expected}\t-\n")
            else:
                with summary_file.open("a") as f:
                    f.write(f"{label}\tmismatch\t{expected}\t{observed}\n")
                failures += 1
        elif expected_lower in ("panic", "panics"):
            # "panic" means we expect a Rust panic/abort without a RUSTEZE violation signature.
            if panicked and observed is None:
                with summary_file.open("a") as f:
                    f.write(f"{label}\tok\t{expected}\tpanic\n")
            else:
                observed_text = observed or ("-" if not panicked else "panic")
                with summary_file.open("a") as f:
                    f.write(f"{label}\tmismatch\t{expected}\t{observed_text}\n")
                failures += 1
        else:
            if observed == expected:
                with summary_file.open("a") as f:
                    f.write(f"{label}\tok\t{expected}\t{observed}\n")
            else:
                with summary_file.open("a") as f:
                    f.write(f"{label}\tmismatch\t{expected}\t{observed or '-'}\n")
                failures += 1

    if ran == 0:
        die(f"No examples matched EXAMPLE_FILTER={example_filter!r}")

    if missing and not allow_missing and not record_expect:
        print(f"Missing expectations: {missing}. Set ALLOW_MISSING_EXPECT=1 to ignore.", file=sys.stderr)

    if failures or (missing and not allow_missing and not record_expect):
        return 1

    print(f"Example tests complete. Summary: {summary_file}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

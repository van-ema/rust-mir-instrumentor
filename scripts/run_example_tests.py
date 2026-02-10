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
        return "sb_lite"
    model = raw.strip().lower()
    if model in ("", "sb", "sb_lite", "stacked_borrows"):
        return "sb_lite"
    if model in ("tb", "tb_lite", "tree_borrows"):
        return "tb_lite"
    if model in ("none", "off"):
        return "none"
    return model


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
    if alias_model == "sb_lite":
        expect_file = pkg_dir / f"expected.{bin_name}.rz"
    else:
        expect_file = pkg_dir / f"expected.{bin_name}.{alias_model}.rz"
    expect_file.write_text(f"{value}\n")


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


def main() -> int:
    env = os.environ.copy()
    env.setdefault("CARGO_INCREMENTAL", "0")
    env.setdefault("RZ_LOG", "warn")
    env.setdefault("RZ_INSTRUMENT_ALL_DEPS", "1")

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

        with instr_log.open("w") as f:
            result = subprocess.run(instr_cmd, env=env, stdout=f, stderr=subprocess.STDOUT)
        if result.returncode != 0:
            with summary_file.open("a") as f:
                f.write(f"{label}\tinstrument_fail\t{expected or 'missing'}\t-\n")
            failures += 1
            continue

        bin_path = bin_dir / bin_name
        with run_log.open("w") as f:
            run_result = subprocess.run([str(bin_path)], env=env, stdout=f, stderr=subprocess.STDOUT)

        observed = extract_signature(run_log)
        panicked = did_panic(run_log) or run_result.returncode != 0

        if expected is None and record_expect:
            expected = observed or "ok"
            write_expectation(pkg_dir, bin_name, alias_model, expected)
        elif record_expect and alias_model != "sb_lite":
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

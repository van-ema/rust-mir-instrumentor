#!/usr/bin/env python3
import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import time
from pathlib import Path


def die(msg: str) -> None:
    print(msg, file=sys.stderr)
    raise SystemExit(2)


def now_stamp() -> str:
    return time.strftime("%Y%m%d_%H%M%S")


def ensure_tool(name: str, env: dict[str, str]) -> str:
    path = shutil.which(name, path=env.get("PATH"))
    if path is None:
        die(f"{name} not found in PATH")
    return path


def strip_rz_env(env: dict[str, str]) -> dict[str, str]:
    out = env.copy()
    for k in list(out.keys()):
        if k.startswith("RZ_") or k.startswith("RUSTEZE_"):
            out.pop(k, None)
    return out


def summarize(samples: list[float]) -> dict[str, float]:
    if not samples:
        return {"n": 0}
    s = sorted(samples)
    n = len(s)
    mean = statistics.fmean(s)
    median = statistics.median(s)
    p90 = s[int(0.90 * (n - 1))]
    p99 = s[int(0.99 * (n - 1))]
    stdev = statistics.pstdev(s) if n > 1 else 0.0
    return {
        "n": float(n),
        "mean_s": mean,
        "median_s": median,
        "p90_s": p90,
        "p99_s": p99,
        "stdev_s": stdev,
    }


def run_timed(cmd: list[str], env: dict[str, str], runs: int, warmup: int, timeout_s: float | None) -> tuple[list[float], list[int]]:
    times: list[float] = []
    codes: list[int] = []

    def one() -> tuple[float, int]:
        start = time.perf_counter()
        proc = subprocess.run(cmd, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=timeout_s)
        end = time.perf_counter()
        return (end - start), proc.returncode

    for _ in range(warmup):
        one()

    for _ in range(runs):
        dt, code = one()
        times.append(dt)
        codes.append(code)

    return times, codes


def build_runtime(cargo: str, env: dict[str, str], profile: str, target_dir: Path) -> Path:
    env = env.copy()
    env["CARGO_TARGET_DIR"] = str(target_dir)
    cmd = [cargo, "build", "-p", "runtime"]
    if profile == "release":
        cmd.append("--release")
    subprocess.run(cmd, env=env, check=True)
    return target_dir / profile


def build_baseline(cargo: str, env: dict[str, str], profile: str, target_dir: Path) -> Path:
    env = env.copy()
    env["CARGO_TARGET_DIR"] = str(target_dir)
    cmd = [cargo, "build", "-p", "rz_bench", "--bin", "rz_bench"]
    if profile == "release":
        cmd.append("--release")
    subprocess.run(cmd, env=env, check=True)
    bin_path = target_dir / profile / "rz_bench"
    if not bin_path.exists():
        die(f"missing baseline binary: {bin_path}")
    return bin_path


def build_rusteze(cargo: str, env: dict[str, str], profile: str, target_dir: Path, runtime_dir: Path) -> Path:
    env = env.copy()
    env["CARGO_TARGET_DIR"] = str(target_dir)
    cmd = [cargo, "instrument-mir", f"--runtime-path={runtime_dir}", "-p", "rz_bench", "--bin", "rz_bench"]
    if profile == "release":
        cmd.append("--release")
    subprocess.run(cmd, env=env, check=True)
    bin_path = target_dir / profile / "rz_bench"
    if not bin_path.exists():
        die(f"missing instrumented binary: {bin_path}")
    return bin_path


def build_asan(cargo: str, env: dict[str, str], profile: str, target_dir: Path, rustflags: str) -> Path:
    env = strip_rz_env(env)
    env = env.copy()
    env["CARGO_TARGET_DIR"] = str(target_dir)
    env["RUSTFLAGS"] = rustflags if not env.get("RUSTFLAGS") else f"{env['RUSTFLAGS']} {rustflags}"
    cmd = [cargo, "+nightly", "build", "-p", "rz_bench", "--bin", "rz_bench"]
    if profile == "release":
        cmd.append("--release")
    subprocess.run(cmd, env=env, check=True)
    bin_path = target_dir / profile / "rz_bench"
    if not bin_path.exists():
        die(f"missing ASan binary: {bin_path}")
    return bin_path


def miri_available(cargo: str, env: dict[str, str]) -> bool:
    try:
        subprocess.run([cargo, "+nightly", "miri", "--version"], env=env, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        return True
    except Exception:
        return False


def run_miri(cargo: str, env: dict[str, str], which: str, iters: int, runs: int, timeout_s: float | None) -> tuple[list[float], list[int]]:
    env = strip_rz_env(env)
    times: list[float] = []
    codes: list[int] = []
    cmd = [cargo, "+nightly", "miri", "run", "-p", "rz_bench", "--bin", "rz_bench", "--", "--iters", str(iters), "--which", which]
    for _ in range(runs):
        start = time.perf_counter()
        proc = subprocess.run(cmd, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=timeout_s)
        end = time.perf_counter()
        times.append(end - start)
        codes.append(proc.returncode)
    return times, codes


def main() -> int:
    ap = argparse.ArgumentParser(description="Compare rz_bench performance: baseline vs rusteze vs ASan (optional Miri).")
    ap.add_argument("--profile", choices=["debug", "release"], default="release")
    ap.add_argument("--which", choices=["bytes", "smallvec", "both"], default="bytes")
    ap.add_argument("--iters", type=int, default=100_000)
    ap.add_argument("--runs", type=int, default=20)
    ap.add_argument("--warmup", type=int, default=3)
    ap.add_argument("--timeout-s", type=float, default=60.0)
    ap.add_argument("--no-timeout", action="store_true")
    ap.add_argument("--include-asan", action="store_true")
    ap.add_argument("--asan-rustflags", default="-Zsanitizer=address")
    ap.add_argument("--include-miri", action="store_true")
    ap.add_argument("--miri-runs", type=int, default=1)
    ap.add_argument("--report-dir", default="reports/bench_compare")
    args = ap.parse_args()

    env = os.environ.copy()
    env.setdefault("CARGO_INCREMENTAL", "0")
    env.setdefault("RZ_LOG", "warn")
    env.setdefault("RZ_INSTRUMENT_ALL_DEPS", "1")

    cargo = env.get("CARGO", "cargo")
    ensure_tool("cargo", env)
    ensure_tool("cargo-instrument-mir", env)
    ensure_tool("instrument-mir", env)

    repo_root = Path(__file__).resolve().parent.parent
    os.chdir(repo_root)

    if args.include_miri and not miri_available(cargo, env):
        die("Miri not available. Install with: rustup +nightly component add miri")

    timeout_s = None if args.no_timeout else args.timeout_s

    bench_root = repo_root / "target" / "bench"
    base_dir = bench_root / f"rzbench-baseline-{args.profile}"
    rz_dir = bench_root / f"rzbench-rusteze-{args.profile}"
    asan_dir = bench_root / f"rzbench-asan-{args.profile}"

    # Build runtimes for instrumentation and warm compilation caches.
    rz_runtime_dir = build_runtime(cargo, env, args.profile, rz_dir)
    _ = build_runtime(cargo, env, args.profile, base_dir)

    base_bin = build_baseline(cargo, strip_rz_env(env), args.profile, base_dir)
    rz_bin = build_rusteze(cargo, env, args.profile, rz_dir, rz_runtime_dir)

    asan_bin = None
    if args.include_asan:
        asan_bin = build_asan(cargo, env, args.profile, asan_dir, args.asan_rustflags)

    base_cmd = [str(base_bin), "--iters", str(args.iters), "--which", args.which]
    rz_cmd = [str(rz_bin), "--iters", str(args.iters), "--which", args.which]
    asan_cmd = None if asan_bin is None else [str(asan_bin), "--iters", str(args.iters), "--which", args.which]

    base_times, base_codes = run_timed(base_cmd, strip_rz_env(env), args.runs, args.warmup, timeout_s)
    rz_env = env.copy()
    rz_env.setdefault("RZ_LOG", "error")
    rz_times, rz_codes = run_timed(rz_cmd, rz_env, args.runs, args.warmup, timeout_s)

    asan_times: list[float] = []
    asan_codes: list[int] = []
    if asan_cmd is not None:
        asan_env = strip_rz_env(env)
        asan_env.setdefault("ASAN_OPTIONS", "detect_leaks=0:abort_on_error=1")
        asan_times, asan_codes = run_timed(asan_cmd, asan_env, args.runs, args.warmup, timeout_s)

    miri_times: list[float] = []
    miri_codes: list[int] = []
    if args.include_miri:
        miri_times, miri_codes = run_miri(cargo, env, args.which, args.iters, args.miri_runs, timeout_s)

    base_sum = summarize(base_times)
    rz_sum = summarize(rz_times)
    asan_sum = summarize(asan_times) if asan_cmd is not None else None
    miri_sum = summarize(miri_times) if args.include_miri else None

    rz_over_base = None
    if base_sum.get("mean_s", 0.0) and rz_sum.get("mean_s", 0.0):
        rz_over_base = rz_sum["mean_s"] / base_sum["mean_s"]

    asan_over_base = None
    rz_over_asan = None
    if asan_sum is not None and base_sum.get("mean_s", 0.0) and asan_sum.get("mean_s", 0.0):
        asan_over_base = asan_sum["mean_s"] / base_sum["mean_s"]
    if asan_sum is not None and asan_sum.get("mean_s", 0.0) and rz_sum.get("mean_s", 0.0):
        rz_over_asan = rz_sum["mean_s"] / asan_sum["mean_s"]

    stamp = now_stamp()
    report_dir = Path(args.report_dir) / stamp
    report_dir.mkdir(parents=True, exist_ok=True)

    result = {
        "profile": args.profile,
        "which": args.which,
        "iters": args.iters,
        "runs": args.runs,
        "warmup": args.warmup,
        "baseline": {"bin": str(base_bin), "cmd": base_cmd, "times_s": base_times, "exit_codes": base_codes, "summary": base_sum},
        "rusteze": {"bin": str(rz_bin), "cmd": rz_cmd, "times_s": rz_times, "exit_codes": rz_codes, "summary": rz_sum},
        "asan": None if asan_cmd is None else {"bin": str(asan_bin), "cmd": asan_cmd, "times_s": asan_times, "exit_codes": asan_codes, "summary": asan_sum},
        "miri": None if not args.include_miri else {"cmd": ["cargo", "+nightly", "miri", "run", "-p", "rz_bench", "--bin", "rz_bench", "--", "--iters", str(args.iters), "--which", args.which], "times_s": miri_times, "exit_codes": miri_codes, "summary": miri_sum},
        "ratios": {
            "rusteze_over_baseline": rz_over_base,
            "asan_over_baseline": asan_over_base,
            "rusteze_over_asan": rz_over_asan,
        },
    }

    (report_dir / "result.json").write_text(json.dumps(result, indent=2))

    header = ["which", "iters", "base_mean_s", "rz_mean_s", "rz_over_base"]
    row = [args.which, str(args.iters), f"{base_sum.get('mean_s', 0.0):.6f}", f"{rz_sum.get('mean_s', 0.0):.6f}", "-" if rz_over_base is None else f"{rz_over_base:.3f}"]
    if asan_sum is not None:
        header += ["asan_mean_s", "asan_over_base", "rz_over_asan"]
        row += [
            f"{asan_sum.get('mean_s', 0.0):.6f}",
            "-" if asan_over_base is None else f"{asan_over_base:.3f}",
            "-" if rz_over_asan is None else f"{rz_over_asan:.3f}",
        ]
    if args.include_miri:
        header += ["miri_mean_s"]
        row += ["-" if miri_sum is None else f"{miri_sum.get('mean_s', 0.0):.6f}"]

    (report_dir / "summary.tsv").write_text("\t".join(header) + "\n" + "\t".join(row) + "\n")
    print(f"Wrote {report_dir/'summary.tsv'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())


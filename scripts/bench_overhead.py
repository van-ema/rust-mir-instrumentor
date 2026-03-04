#!/usr/bin/env python3
import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import time
from dataclasses import dataclass
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


@dataclass(frozen=True)
class Target:
    pkg: str
    bin: str
    pkg_dir: Path
    required_features: tuple[str, ...] = ()

    @property
    def label(self) -> str:
        return self.pkg if self.bin == self.pkg else f"{self.pkg}::{self.bin}"


def load_targets(cargo: str, env: dict[str, str], suite: str) -> list[Target]:
    out = subprocess.check_output(
        [cargo, "metadata", "--no-deps", "--format-version", "1"],
        text=True,
        env=env,
    )
    data = json.loads(out)

    want_segment = f"/{suite}/"
    targets: list[Target] = []

    for pkg in data.get("packages", []):
        manifest_path = pkg.get("manifest_path", "")
        if suite == "fuzz":
            # Suite name doesn't match directory layout; map to fuzz/ explicitly.
            if "/fuzz/" not in manifest_path:
                continue
        elif want_segment not in manifest_path:
            continue
        pkg_dir = Path(manifest_path).parent
        for tgt in pkg.get("targets", []):
            if "bin" in tgt.get("kind", []):
                targets.append(
                    Target(
                        pkg=pkg["name"],
                        bin=tgt["name"],
                        pkg_dir=pkg_dir,
                        required_features=tuple(tgt.get("required-features", [])),
                    )
                )

    return sorted(targets, key=lambda t: t.label)


def parse_target_spec(spec: str, repo_root: Path, cargo: str, env: dict[str, str]) -> Target:
    # spec is "pkg" or "pkg::bin"
    if "::" in spec:
        pkg, bin_name = spec.split("::", 1)
    else:
        pkg, bin_name = spec, spec

    out = subprocess.check_output(
        [cargo, "metadata", "--no-deps", "--format-version", "1"],
        text=True,
        env=env,
    )
    data = json.loads(out)
    for pkg_meta in data.get("packages", []):
        if pkg_meta.get("name") != pkg:
            continue
        pkg_dir = Path(pkg_meta["manifest_path"]).parent
        for tgt in pkg_meta.get("targets", []):
            if "bin" in tgt.get("kind", []) and tgt.get("name") == bin_name:
                return Target(
                    pkg=pkg,
                    bin=bin_name,
                    pkg_dir=pkg_dir,
                    required_features=tuple(tgt.get("required-features", [])),
                )
        die(f"package {pkg} has no bin target {bin_name}")
    die(f"unknown package {pkg}")


def add_required_features(cmd: list[str], target: Target) -> None:
    if target.required_features:
        cmd.extend(["--features", ",".join(target.required_features)])


def cargo_build_runtime(cargo: str, env: dict[str, str], profile: str, target_dir: Path) -> Path:
    env = env.copy()
    env["CARGO_TARGET_DIR"] = str(target_dir)

    cmd = [cargo, "build", "-p", "runtime"]
    if profile == "release":
        cmd.append("--release")
    subprocess.run(cmd, env=env, check=True)

    return target_dir / profile


def cargo_build_baseline(
    cargo: str,
    env: dict[str, str],
    profile: str,
    target_dir: Path,
    target: Target,
) -> Path:
    env = env.copy()
    env["CARGO_TARGET_DIR"] = str(target_dir)

    cmd = [cargo, "build", "-p", target.pkg, "--bin", target.bin]
    add_required_features(cmd, target)
    if profile == "release":
        cmd.append("--release")
    subprocess.run(cmd, env=env, check=True)

    bin_path = target_dir / profile / target.bin
    if not bin_path.exists():
        die(f"baseline binary not found: {bin_path}")
    return bin_path


def cargo_build_asan(
    cargo: str,
    env: dict[str, str],
    profile: str,
    target_dir: Path,
    target: Target,
    rustflags: str,
    asan_dylib: Path | None,
) -> Path:
    env = strip_rz_env(env)
    env = env.copy()
    env["CARGO_TARGET_DIR"] = str(target_dir)
    env["RUSTFLAGS"] = rustflags if not env.get("RUSTFLAGS") else f"{env['RUSTFLAGS']} {rustflags}"
    if asan_dylib is not None:
        env.setdefault("DYLD_INSERT_LIBRARIES", str(asan_dylib))

    cmd = [cargo, "+nightly", "build", "-p", target.pkg, "--bin", target.bin]
    add_required_features(cmd, target)
    if profile == "release":
        cmd.append("--release")
    subprocess.run(cmd, env=env, check=True)

    bin_path = target_dir / profile / target.bin
    if not bin_path.exists():
        die(f"asan binary not found: {bin_path}")
    return bin_path


def cargo_miri_available(cargo: str, env: dict[str, str]) -> bool:
    try:
        subprocess.run([cargo, "+nightly", "miri", "--version"], env=env, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        return True
    except Exception:
        return False


def resolve_macos_asan_dylib(env: dict[str, str]) -> Path | None:
    if sys.platform != "darwin":
        return None
    try:
        libdir = subprocess.check_output(
            ["rustc", "+nightly", "--print", "target-libdir"],
            text=True,
            env=env,
        ).strip()
    except Exception:
        return None

    libdir_path = Path(libdir)
    direct = libdir_path / "librustc-nightly_rt.asan.dylib"
    if direct.exists():
        return direct

    matches = sorted(libdir_path.glob("librustc*_rt.asan.dylib"))
    return matches[0] if matches else None


def run_miri(
    cargo: str,
    env: dict[str, str],
    target: Target,
    runs: int,
    timeout_s: float | None,
    bin_args: list[str] | None = None,
) -> tuple[list[float], list[int]]:
    times: list[float] = []
    codes: list[int] = []
    env = strip_rz_env(env)
    cmd = [cargo, "+nightly", "miri", "run", "-p", target.pkg, "--bin", target.bin]
    add_required_features(cmd, target)
    if bin_args:
        cmd += ["--", *bin_args]
    for _ in range(runs):
        start = time.perf_counter()
        proc = subprocess.run(
            cmd,
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=timeout_s,
        )
        end = time.perf_counter()
        times.append(end - start)
        codes.append(proc.returncode)
    return times, codes


def cargo_build_rusteze(
    cargo: str,
    env: dict[str, str],
    profile: str,
    target_dir: Path,
    target: Target,
    runtime_dir: Path,
    mir_out: Path | None,
) -> Path:
    env = env.copy()
    env["CARGO_TARGET_DIR"] = str(target_dir)

    cmd = [
        cargo,
        "instrument-mir",
        f"--runtime-path={runtime_dir}",
        "-p",
        target.pkg,
        "--bin",
        target.bin,
    ]
    add_required_features(cmd, target)
    if mir_out is not None:
        cmd.insert(3, f"--mir-out={mir_out}")
    if profile == "release":
        cmd.append("--release")

    subprocess.run(cmd, env=env, check=True)

    bin_path = target_dir / profile / target.bin
    if not bin_path.exists():
        die(f"instrumented binary not found: {bin_path}")
    return bin_path


def run_timed(
    bin_path: Path,
    env: dict[str, str],
    runs: int,
    warmup: int,
    timeout_s: float | None,
    bin_args: list[str] | None = None,
) -> tuple[list[float], list[int]]:
    times: list[float] = []
    exit_codes: list[int] = []
    cmd = [str(bin_path)]
    if bin_args:
        cmd.extend(bin_args)

    def one_run() -> tuple[float, int]:
        start = time.perf_counter()
        proc = subprocess.run(
            cmd,
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=timeout_s,
        )
        end = time.perf_counter()
        return (end - start), proc.returncode

    for _ in range(warmup):
        one_run()

    for _ in range(runs):
        dt, code = one_run()
        times.append(dt)
        exit_codes.append(code)

    return times, exit_codes


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

def fmt_s(x: float | None) -> str:
    if x is None:
        return "-"
    return f"{x:.6f}"


def main() -> int:
    ap = argparse.ArgumentParser(description="Measure runtime overhead (baseline vs rusteze).")
    ap.add_argument("--profile", choices=["debug", "release"], default="release")
    ap.add_argument("--suite", choices=["examples", "medium", "fuzz"], default="examples")
    ap.add_argument("--targets", nargs="*", default=[], help="Explicit targets as pkg or pkg::bin")
    ap.add_argument("--runs", type=int, default=30)
    ap.add_argument("--warmup", type=int, default=3)
    ap.add_argument("--timeout-s", type=float, default=20.0)
    ap.add_argument("--no-timeout", action="store_true")
    ap.add_argument("--report-dir", default="reports/overhead")
    ap.add_argument("--emit-mir", action="store_true", help="Write MIR output per target (debugging)")
    ap.add_argument(
        "--input-file",
        default="",
        help="Optional input file path passed as argv[1] to each benchmark binary.",
    )
    ap.add_argument("--include-asan", action="store_true", help="Also build+run AddressSanitizer (nightly)")
    ap.add_argument(
        "--asan-rustflags",
        default="-Zsanitizer=address",
        help="Extra rustc flags for ASan builds (default: -Zsanitizer=address)",
    )
    ap.add_argument("--include-miri", action="store_true", help="Also run under Miri (nightly; very slow)")
    ap.add_argument("--miri-runs", type=int, default=1, help="Number of Miri runs per target (default: 1)")
    args = ap.parse_args()

    env = os.environ.copy()
    env.setdefault("CARGO_INCREMENTAL", "0")
    env.setdefault("RZ_LOG", "warn")
    env.setdefault("RZ_INSTRUMENT_ALL_DEPS", "1")

    ensure_tool("cargo", env)
    ensure_tool("cargo-instrument-mir", env)
    ensure_tool("instrument-mir", env)

    script_dir = Path(__file__).resolve().parent
    repo_root = script_dir.parent
    os.chdir(repo_root)

    cargo = env.get("CARGO", "cargo")

    # Isolate builds to avoid mixing baseline and instrumented artifacts.
    bench_root = repo_root / "target" / "bench"
    base_target_dir = bench_root / f"baseline-{args.profile}"
    rz_target_dir = bench_root / f"rusteze-{args.profile}"
    asan_target_dir = bench_root / f"asan-{args.profile}"

    base_runtime_dir = cargo_build_runtime(cargo, env, args.profile, base_target_dir)
    rz_runtime_dir = cargo_build_runtime(cargo, env, args.profile, rz_target_dir)
    _ = base_runtime_dir  # baseline does not need runtime, but building once warms dependencies.

    if args.include_asan:
        # Quick sanity check early so we fail fast with a clear message.
        subprocess.run([cargo, "+nightly", "--version"], env=env, check=True, stdout=subprocess.DEVNULL)
    asan_dylib = resolve_macos_asan_dylib(env) if args.include_asan else None

    if args.include_miri:
        if not cargo_miri_available(cargo, env):
            die("Miri not available. Install with: rustup +nightly component add miri")

    targets: list[Target] = []
    if args.targets:
        for spec in args.targets:
            targets.append(parse_target_spec(spec, repo_root, cargo, env))
    else:
        targets = load_targets(cargo, env, args.suite)

    if not targets:
        die("no targets selected")

    stamp = now_stamp()
    report_dir = Path(args.report_dir) / stamp
    report_dir.mkdir(parents=True, exist_ok=True)

    results: list[dict] = []

    timeout_s = None if args.no_timeout else args.timeout_s
    bin_args: list[str] = []
    if args.input_file:
        input_path = Path(args.input_file)
        if not input_path.is_absolute():
            input_path = (repo_root / input_path).resolve()
        if not input_path.exists():
            die(f"input file does not exist: {input_path}")
        bin_args = [str(input_path)]

    for t in targets:
        base_bin = cargo_build_baseline(cargo, env, args.profile, base_target_dir, t)
        asan_bin = None
        if args.include_asan:
            asan_bin = cargo_build_asan(
                cargo,
                env,
                args.profile,
                asan_target_dir,
                t,
                rustflags=args.asan_rustflags,
                asan_dylib=asan_dylib,
            )

        mir_out = None
        if args.emit_mir:
            mir_out = report_dir / f"out.{t.pkg}__{t.bin}.mir"
        rz_bin = cargo_build_rusteze(
            cargo,
            env,
            args.profile,
            rz_target_dir,
            t,
            rz_runtime_dir,
            mir_out,
        )

        base_times, base_codes = run_timed(
            base_bin,
            env=strip_rz_env(env),
            runs=args.runs,
            warmup=args.warmup,
            timeout_s=timeout_s,
            bin_args=bin_args,
        )
        rz_env = env.copy()
        # Reduce logging overhead during benchmarks.
        rz_env.setdefault("RZ_LOG", "error")
        rz_times, rz_codes = run_timed(
            rz_bin,
            env=rz_env,
            runs=args.runs,
            warmup=args.warmup,
            timeout_s=timeout_s,
            bin_args=bin_args,
        )
        asan_times: list[float] = []
        asan_codes: list[int] = []
        if asan_bin is not None:
            asan_env = strip_rz_env(env)
            # Reduce ASan overhead variance/noise (especially on macOS).
            asan_env.setdefault("ASAN_OPTIONS", "detect_leaks=0")
            if asan_dylib is not None:
                asan_env.setdefault("DYLD_INSERT_LIBRARIES", str(asan_dylib))
            asan_times, asan_codes = run_timed(
                asan_bin,
                env=asan_env,
                runs=args.runs,
                warmup=args.warmup,
                timeout_s=timeout_s,
                bin_args=bin_args,
            )

        miri_times: list[float] = []
        miri_codes: list[int] = []
        if args.include_miri:
            # Miri does not have a meaningful "release" mode; it interprets MIR and is orders of
            # magnitude slower. Include it for completeness, but do not interpret as overhead.
            miri_times, miri_codes = run_miri(
                cargo,
                env,
                t,
                runs=args.miri_runs,
                timeout_s=timeout_s,
                bin_args=bin_args,
            )

        base_sum = summarize(base_times)
        rz_sum = summarize(rz_times)
        asan_sum = summarize(asan_times) if asan_bin is not None else None
        miri_sum = summarize(miri_times) if args.include_miri else None
        overhead = None
        if base_sum.get("mean_s", 0.0) > 0.0 and rz_sum.get("mean_s", 0.0) > 0.0:
            overhead = rz_sum["mean_s"] / base_sum["mean_s"]
        asan_overhead = None
        if asan_sum is not None and base_sum.get("mean_s", 0.0) > 0.0 and asan_sum.get("mean_s", 0.0) > 0.0:
            asan_overhead = asan_sum["mean_s"] / base_sum["mean_s"]
        rz_vs_asan = None
        if asan_sum is not None and asan_sum.get("mean_s", 0.0) > 0.0 and rz_sum.get("mean_s", 0.0) > 0.0:
            rz_vs_asan = rz_sum["mean_s"] / asan_sum["mean_s"]

        results.append(
            {
                "target": t.label,
                "pkg": t.pkg,
                "bin": t.bin,
                "profile": args.profile,
                "input_file": bin_args[0] if bin_args else None,
                "baseline": {"bin": str(base_bin), "times_s": base_times, "exit_codes": base_codes, "summary": base_sum},
                "rusteze": {"bin": str(rz_bin), "times_s": rz_times, "exit_codes": rz_codes, "summary": rz_sum},
                "asan": (
                    None
                    if asan_bin is None
                    else {"bin": str(asan_bin), "times_s": asan_times, "exit_codes": asan_codes, "summary": asan_sum}
                ),
                "miri": (
                    None
                    if not args.include_miri
                    else {"times_s": miri_times, "exit_codes": miri_codes, "summary": miri_sum}
                ),
                "overhead_mean_ratio_rusteze_over_baseline": overhead,
                "overhead_mean_ratio_asan_over_baseline": asan_overhead,
                "overhead_mean_ratio_rusteze_over_asan": rz_vs_asan,
            }
        )

    (report_dir / "results.json").write_text(json.dumps(results, indent=2))

    header = ["target", "base_mean_s", "rz_mean_s"]
    if args.include_asan:
        header += ["asan_mean_s", "rz_over_base", "asan_over_base", "rz_over_asan"]
    else:
        header += ["rz_over_base"]
    if args.include_miri:
        header += ["miri_mean_s"]
    lines = ["\t".join(header)]

    for r in results:
        base_mean = r["baseline"]["summary"].get("mean_s", 0.0)
        rz_mean = r["rusteze"]["summary"].get("mean_s", 0.0)
        rz_over_base = r.get("overhead_mean_ratio_rusteze_over_baseline")
        rz_over_base_s = "-" if rz_over_base is None else f"{rz_over_base:.3f}"
        if args.include_asan:
            asan = r.get("asan")
            asan_mean = 0.0 if asan is None else asan["summary"].get("mean_s", 0.0)
            asan_over_base = r.get("overhead_mean_ratio_asan_over_baseline")
            asan_over_base_s = "-" if asan_over_base is None else f"{asan_over_base:.3f}"
            rz_over_asan = r.get("overhead_mean_ratio_rusteze_over_asan")
            rz_over_asan_s = "-" if rz_over_asan is None else f"{rz_over_asan:.3f}"
            row = [r["target"], f"{base_mean:.6f}", f"{rz_mean:.6f}", f"{asan_mean:.6f}", rz_over_base_s, asan_over_base_s, rz_over_asan_s]
        else:
            row = [r["target"], f"{base_mean:.6f}", f"{rz_mean:.6f}", rz_over_base_s]

        if args.include_miri:
            miri = r.get("miri")
            miri_mean = "-" if miri is None else f"{miri['summary'].get('mean_s', 0.0):.6f}"
            row.append(miri_mean)

        lines.append("\t".join(row))
    (report_dir / "summary.tsv").write_text("\n".join(lines) + "\n")

    md = [
        "# Overhead summary",
        "",
        f"- profile: `{args.profile}`",
        f"- suite: `{args.suite}`",
        f"- runs: `{args.runs}`",
        f"- warmup: `{args.warmup}`",
    ]
    if args.include_miri:
        md.append("- miri: included (not comparable to native runtime)")
    md.append("")

    if args.include_asan:
        header = "| target | baseline mean (s) | rusteze mean (s) | ASan mean (s) | rusteze/baseline (x) | ASan/baseline (x) | rusteze/ASan (x) |"
        sep = "|---|---:|---:|---:|---:|---:|---:|"
        if args.include_miri:
            header = header[:-1] + " Miri mean (s) |"
            sep = sep[:-1] + "---:|"
        md.append(header)
        md.append(sep)
    else:
        header = "| target | baseline mean (s) | rusteze mean (s) | rusteze/baseline (x) |"
        sep = "|---|---:|---:|---:|"
        if args.include_miri:
            header = header[:-1] + " Miri mean (s) |"
            sep = sep[:-1] + "---:|"
        md.append(header)
        md.append(sep)
    for r in results:
        base_mean = r["baseline"]["summary"].get("mean_s")
        rz_mean = r["rusteze"]["summary"].get("mean_s")
        rz_over_base = r.get("overhead_mean_ratio_rusteze_over_baseline")
        if args.include_asan:
            asan = r.get("asan")
            asan_mean = None if asan is None else asan["summary"].get("mean_s")
            asan_over_base = r.get("overhead_mean_ratio_asan_over_baseline")
            rz_over_asan = r.get("overhead_mean_ratio_rusteze_over_asan")
            row = (
                f"| `{r['target']}` | {fmt_s(base_mean)} | {fmt_s(rz_mean)} | {fmt_s(asan_mean)} | "
                f"{'-' if rz_over_base is None else f'{rz_over_base:.3f}'} | "
                f"{'-' if asan_over_base is None else f'{asan_over_base:.3f}'} | "
                f"{'-' if rz_over_asan is None else f'{rz_over_asan:.3f}'} |"
            )
        else:
            row = f"| `{r['target']}` | {fmt_s(base_mean)} | {fmt_s(rz_mean)} | {'-' if rz_over_base is None else f'{rz_over_base:.3f}'} |"

        if args.include_miri:
            miri = r.get("miri")
            miri_mean = None if miri is None else miri["summary"].get("mean_s")
            row = row[:-1] + f" {fmt_s(miri_mean)} |"

        md.append(row)
    (report_dir / "summary.md").write_text("\n".join(md) + "\n")

    print(f"Wrote {report_dir/'summary.tsv'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

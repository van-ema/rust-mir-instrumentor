#!/usr/bin/env python3
import argparse
import json
from copy import deepcopy
from pathlib import Path


DIRECT_ESCAPE = 0x10000
FORWARD_TO_RETURN = 0x20000
INHERITED_ESCAPE = 0x40000


def load_dir_records(input_dir: Path):
    records = {}
    order_by_crate = {}
    filename_by_crate = {}

    for path in sorted(input_dir.glob("*.jsonl")):
        crate_records = []
        with path.open() as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                rec = json.loads(line)
                key = (rec["crate_name"], rec["function"])
                records[key] = rec
                crate_records.append(key)
                filename_by_crate.setdefault(rec["crate_name"], path.name)
        if crate_records:
            crate_name = crate_records[0][0]
            order_by_crate.setdefault(crate_name, []).extend(crate_records)

    return records, order_by_crate, filename_by_crate


def load_single_file(path: Path):
    records = {}
    order = []
    with path.open() as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            rec = json.loads(line)
            key = (rec["crate_name"], rec["function"])
            records[key] = rec
            order.append(key)
    crate_name = order[0][0] if order else path.stem
    return records, {crate_name: order}, {crate_name: path.name}


def ensure_arg(summary, arg_index):
    ptr_args = summary["ptr_args"]
    for arg in ptr_args:
        if arg["arg_index"] == arg_index:
            return arg
    arg = {
        "arg_index": arg_index,
        "direct_sink_mask": 0,
        "propagation_mask": 0,
    }
    ptr_args.append(arg)
    ptr_args.sort(key=lambda e: e["arg_index"])
    return arg


def callee_key(call):
    return (call["callee_crate_name"], call["callee_function"])


def propagate_once(records):
    changed = False
    for key, rec in records.items():
        direct_sink = rec["has_direct_sink"]
        calls_unknown_direct = rec.get(
            "calls_unknown_boundary_direct", rec.get("calls_unknown_boundary", False)
        )
        calls_unknown_inherited = rec.get("calls_unknown_boundary_inherited", False)
        ptr_args = deepcopy(rec["ptr_args"])
        scratch = {"ptr_args": ptr_args}

        for call in rec.get("local_callsites", []):
            callee = records.get(callee_key(call))
            if callee is None:
                continue
            callee_unknown = (
                callee.get(
                    "calls_unknown_boundary_direct",
                    callee.get("calls_unknown_boundary", False),
                )
                or callee.get("calls_unknown_boundary_inherited", False)
            )
            calls_unknown_inherited = calls_unknown_inherited or callee_unknown
            callee_args = {
                entry["arg_index"]: entry for entry in callee.get("ptr_args", [])
            }
            for edge in call.get("arg_edges", []):
                caller_arg = ensure_arg(scratch, edge["caller_arg_index"])
                callee_arg = callee_args.get(edge["callee_arg_index"])
                if callee_arg is None:
                    continue
                old_direct = caller_arg["direct_sink_mask"]
                old_prop = caller_arg["propagation_mask"]
                caller_arg["direct_sink_mask"] |= callee_arg["direct_sink_mask"]
                if callee_arg["propagation_mask"] & (DIRECT_ESCAPE | INHERITED_ESCAPE):
                    caller_arg["propagation_mask"] |= INHERITED_ESCAPE
                if call.get("return_to_return", False):
                    caller_arg["propagation_mask"] |= (
                        callee_arg["propagation_mask"] & FORWARD_TO_RETURN
                    )
                if caller_arg["direct_sink_mask"] != old_direct:
                    direct_sink = True
                if caller_arg["propagation_mask"] != old_prop:
                    changed = True
            if callee["has_direct_sink"]:
                direct_sink = True

        if direct_sink != rec["has_direct_sink"]:
            rec["has_direct_sink"] = direct_sink
            changed = True
        if calls_unknown_direct != rec.get(
            "calls_unknown_boundary_direct", rec.get("calls_unknown_boundary", False)
        ):
            rec["calls_unknown_boundary_direct"] = calls_unknown_direct
            changed = True
        if calls_unknown_inherited != rec.get("calls_unknown_boundary_inherited", False):
            rec["calls_unknown_boundary_inherited"] = calls_unknown_inherited
            changed = True
        combined_unknown = calls_unknown_direct or calls_unknown_inherited
        if combined_unknown != rec.get("calls_unknown_boundary", combined_unknown):
            rec["calls_unknown_boundary"] = combined_unknown
            changed = True
        if ptr_args != rec["ptr_args"]:
            rec["ptr_args"] = ptr_args
            changed = True
    return changed


def enrich_derived_fields(rec):
    rec["calls_unknown_boundary"] = rec.get(
        "calls_unknown_boundary_direct", rec.get("calls_unknown_boundary", False)
    ) or rec.get("calls_unknown_boundary_inherited", False)
    for arg in rec.get("ptr_args", []):
        arg["reaches_direct_sink"] = arg["direct_sink_mask"] != 0
        arg["escapes_to_unknown_boundary"] = (
            arg["propagation_mask"] & (DIRECT_ESCAPE | INHERITED_ESCAPE)
        ) != 0
        arg["escapes_to_direct_unknown_boundary"] = (
            arg["propagation_mask"] & DIRECT_ESCAPE
        ) != 0
        arg["escapes_to_inherited_unknown_boundary"] = (
            arg["propagation_mask"] & INHERITED_ESCAPE
        ) != 0
        arg["forwarded_to_return"] = (arg["propagation_mask"] & FORWARD_TO_RETURN) != 0


def write_records(records, order_by_crate, filename_by_crate, output_dir: Path):
    output_dir.mkdir(parents=True, exist_ok=True)
    for crate_name, order in order_by_crate.items():
        out_path = output_dir / filename_by_crate[crate_name]
        with out_path.open("w") as f:
            for key in order:
                rec = deepcopy(records[key])
                enrich_derived_fields(rec)
                f.write(json.dumps(rec, sort_keys=True))
                f.write("\n")


def write_report(base_records, merged_records, order_by_crate, report_path: Path):
    report_path.parent.mkdir(parents=True, exist_ok=True)
    with report_path.open("w") as f:
        for crate_name, order in order_by_crate.items():
            for key in order:
                base = base_records[key]
                merged = merged_records[key]
                lines = []
                if base["has_direct_sink"] != merged["has_direct_sink"]:
                    lines.append("function.has_direct_sink")
                if base.get(
                    "calls_unknown_boundary_direct",
                    base.get("calls_unknown_boundary", False),
                ) != merged.get(
                    "calls_unknown_boundary_direct",
                    merged.get("calls_unknown_boundary", False),
                ):
                    lines.append("function.calls_unknown_boundary_direct")
                if base.get("calls_unknown_boundary_inherited", False) != merged.get(
                    "calls_unknown_boundary_inherited", False
                ):
                    lines.append("function.calls_unknown_boundary_inherited")

                base_args = {a["arg_index"]: a for a in base.get("ptr_args", [])}
                merged_args = {a["arg_index"]: a for a in merged.get("ptr_args", [])}
                for arg_index in sorted(set(base_args) | set(merged_args)):
                    b = base_args.get(
                        arg_index, {"direct_sink_mask": 0, "propagation_mask": 0}
                    )
                    m = merged_args.get(
                        arg_index, {"direct_sink_mask": 0, "propagation_mask": 0}
                    )
                    direct_added = m["direct_sink_mask"] & ~b["direct_sink_mask"]
                    prop_added = m["propagation_mask"] & ~b["propagation_mask"]
                    if direct_added:
                        lines.append(f"arg[{arg_index}].direct_sink_mask+=0x{direct_added:x}")
                    if prop_added & DIRECT_ESCAPE:
                        lines.append(
                            f"arg[{arg_index}].propagation.escape_unknown_direct"
                        )
                    if prop_added & INHERITED_ESCAPE:
                        lines.append(
                            f"arg[{arg_index}].propagation.escape_unknown_inherited"
                        )
                    if prop_added & FORWARD_TO_RETURN:
                        lines.append(f"arg[{arg_index}].propagation.forward_to_return")
                if lines:
                    f.write(f"{crate_name}::{key[1]}\n")
                    for line in lines:
                        f.write(f"  - {line}\n")


def main():
    ap = argparse.ArgumentParser(
        description="Merge unsafe summaries to a fixed point across one file or a whole directory"
    )
    ap.add_argument("--input", help="input JSONL file for one crate")
    ap.add_argument(
        "--input-dir", help="input directory containing per-crate summary JSONL files"
    )
    ap.add_argument("--output", help="output JSONL file (single-file mode)")
    ap.add_argument(
        "--output-dir", help="output directory for merged per-crate JSONL files"
    )
    ap.add_argument(
        "--report",
        help="optional report path describing which function/arg summary bits changed",
    )
    args = ap.parse_args()

    if bool(args.input) == bool(args.input_dir):
        ap.error("choose exactly one of --input or --input-dir")
    if bool(args.output) == bool(args.output_dir):
        ap.error("choose exactly one of --output or --output-dir")

    if args.input_dir:
        records, order_by_crate, filename_by_crate = load_dir_records(Path(args.input_dir))
        output_dir = Path(args.output_dir)
    else:
        records, order_by_crate, filename_by_crate = load_single_file(Path(args.input))
        output_dir = Path(args.output_dir) if args.output_dir else Path(args.output).parent
        if args.output:
            crate_name = next(iter(order_by_crate))
            filename_by_crate[crate_name] = Path(args.output).name

    base_records = deepcopy(records)
    while propagate_once(records):
        pass

    write_records(records, order_by_crate, filename_by_crate, output_dir)

    if args.report:
        write_report(base_records, records, order_by_crate, Path(args.report))


if __name__ == "__main__":
    main()

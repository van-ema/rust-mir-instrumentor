#!/usr/bin/env python3
import argparse
import json
from copy import deepcopy
from pathlib import Path


def load_records(path: Path):
    records = {}
    order = []
    with path.open() as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            rec = json.loads(line)
            fn = rec["function"]
            records[fn] = rec
            order.append(fn)
    return records, order


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


def propagate_once(records):
    changed = False
    for fn, rec in records.items():
        direct_sink = rec["has_direct_sink"]
        calls_unknown_direct = rec.get("calls_unknown_boundary_direct", rec.get("calls_unknown_boundary", False))
        calls_unknown_inherited = rec.get("calls_unknown_boundary_inherited", False)
        ptr_args = deepcopy(rec["ptr_args"])
        scratch = {
            "ptr_args": ptr_args,
        }

        for call in rec.get("local_callsites", []):
            callee = records.get(call["callee_function"])
            if callee is None:
                continue
            callee_unknown = (
                callee.get("calls_unknown_boundary_direct", callee.get("calls_unknown_boundary", False))
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
                if callee_arg["propagation_mask"] & (0x10000 | 0x40000):
                    caller_arg["propagation_mask"] |= 0x40000
                if call.get("return_to_return", False):
                    caller_arg["propagation_mask"] |= callee_arg["propagation_mask"] & 0x20000
                if caller_arg["direct_sink_mask"] != old_direct:
                    direct_sink = True
                if caller_arg["propagation_mask"] != old_prop:
                    changed = True
            if callee["has_direct_sink"]:
                direct_sink = True

        if direct_sink != rec["has_direct_sink"]:
            rec["has_direct_sink"] = direct_sink
            changed = True
        if calls_unknown_direct != rec.get("calls_unknown_boundary_direct", rec.get("calls_unknown_boundary", False)):
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
    for arg in rec.get("ptr_args", []):
        arg["reaches_direct_sink"] = arg["direct_sink_mask"] != 0
        arg["escapes_to_unknown_boundary"] = (arg["propagation_mask"] & (0x10000 | 0x40000)) != 0
        arg["escapes_to_direct_unknown_boundary"] = (arg["propagation_mask"] & 0x10000) != 0
        arg["escapes_to_inherited_unknown_boundary"] = (arg["propagation_mask"] & 0x40000) != 0
        arg["forwarded_to_return"] = (arg["propagation_mask"] & 0x20000) != 0


def main():
    ap = argparse.ArgumentParser(description="Merge crate-local unsafe summaries to a fixed point")
    ap.add_argument("--input", required=True, help="input JSONL file for one crate")
    ap.add_argument("--output", required=True, help="output JSONL file with propagated summaries")
    ap.add_argument(
        "--report",
        help="optional report path describing which function/arg summary bits changed",
    )
    args = ap.parse_args()

    records, order = load_records(Path(args.input))
    base_records = deepcopy(records)
    while propagate_once(records):
        pass

    out_path = Path(args.output)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    with out_path.open("w") as f:
        for fn in order:
            rec = deepcopy(records[fn])
            enrich_derived_fields(rec)
            f.write(json.dumps(rec, sort_keys=True))
            f.write("\n")

    if args.report:
        report_path = Path(args.report)
        report_path.parent.mkdir(parents=True, exist_ok=True)
        with report_path.open("w") as f:
            for fn in order:
                base = base_records[fn]
                merged = records[fn]
                lines = []
                if base["has_direct_sink"] != merged["has_direct_sink"]:
                    lines.append("function.has_direct_sink")
                if base.get("calls_unknown_boundary_direct", base.get("calls_unknown_boundary", False)) != merged.get("calls_unknown_boundary_direct", merged.get("calls_unknown_boundary", False)):
                    lines.append("function.calls_unknown_boundary_direct")
                if base.get("calls_unknown_boundary_inherited", False) != merged.get("calls_unknown_boundary_inherited", False):
                    lines.append("function.calls_unknown_boundary_inherited")

                base_args = {a["arg_index"]: a for a in base.get("ptr_args", [])}
                merged_args = {a["arg_index"]: a for a in merged.get("ptr_args", [])}
                for arg_index in sorted(set(base_args) | set(merged_args)):
                    b = base_args.get(arg_index, {"direct_sink_mask": 0, "propagation_mask": 0})
                    m = merged_args.get(arg_index, {"direct_sink_mask": 0, "propagation_mask": 0})
                    direct_added = m["direct_sink_mask"] & ~b["direct_sink_mask"]
                    prop_added = m["propagation_mask"] & ~b["propagation_mask"]
                    if direct_added:
                        lines.append(f"arg[{arg_index}].direct_sink_mask+=0x{direct_added:x}")
                    if prop_added & 0x10000:
                        lines.append(f"arg[{arg_index}].propagation.escape_unknown_direct")
                    if prop_added & 0x40000:
                        lines.append(f"arg[{arg_index}].propagation.escape_unknown_inherited")
                    if prop_added & 0x20000:
                        lines.append(f"arg[{arg_index}].propagation.forward_to_return")
                if lines:
                    f.write(f"{fn}\n")
                    for line in lines:
                        f.write(f"  - {line}\n")


if __name__ == "__main__":
    main()

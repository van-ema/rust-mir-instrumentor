// src/main.rs
#![allow(unused)]
#![feature(rustc_private)]
#![feature(box_patterns)]
extern crate rustc_abi;
extern crate rustc_driver;
extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_mir_transform;
use std::collections::{HashMap, HashSet};
extern crate rustc_session;
extern crate rustc_span;
extern crate rustc_target;

use rustc_abi::ExternAbi;
use rustc_errors::{emitter::HumanReadableErrorType, ColorConfig};
use rustc_hir::def_id::{DefId, DefIndex, LocalDefId, CRATE_DEF_INDEX, LOCAL_CRATE};
use rustc_interface::util::rustc_path;
use rustc_interface::Config;
use rustc_middle::mir::interpret::{AllocId, Scalar};
use rustc_middle::mir::*;
use rustc_middle::mir::{Const, ConstOperand, ConstValue};
use rustc_middle::ty::{self, print, ParamEnv, PseudoCanonicalInput, Ty, TyCtxt, TypingEnv};
use rustc_session::config::ErrorOutputType;
use rustc_session::EarlyDiagCtxt;
use rustc_span::{source_map::Spanned, Span};

use rustc_hir::Safety;
use rustc_middle::middle::exported_symbols::ExportedSymbol;
use rustc_middle::mir::pretty::write_mir_fn;
use rustc_middle::ty::TyKind;
use rustc_span::symbol::Symbol;
use std::num::NonZeroU64;
use std::sync::Mutex;

use std::fs::File;
use std::io::BufWriter;
use std::io::Write;
use std::sync::OnceLock;

static MIR_OUT_BEFORE: OnceLock<String> = OnceLock::new();
static MIR_OUT_AFTER: OnceLock<String> = OnceLock::new();

fn prefixed_path(base: &str, prefix: &str) -> String {
    use std::path::{Path, PathBuf};

    let p = Path::new(base);
    let parent = p.parent().unwrap_or_else(|| Path::new(""));
    let file_name = p
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "mir.txt".to_string());

    let mut out: PathBuf = parent.to_path_buf();
    out.push(format!("{}{}", prefix, file_name));
    out.to_string_lossy().to_string()
}

struct MyOptimizationPass;

impl MyOptimizationPass {
    /*
    fn print_runtime_items<'tcx>(&self, tcx: TyCtxt<'tcx>) {
        for &cnum in tcx.crates(()).iter() {
            let crate_name = tcx.crate_name(cnum);
            if crate_name.as_str() == "runtime" {
                println!("Items in runtime crate:");
                let items = tcx.hir_crate_items(());
                // Use `free_items` to iterate over non-associated items
                for item_id in items.free_items() {
                    let def_id = item_id.owner_id.def_id;
                    if let Some(name) = tcx.opt_item_name(def_id) {
                        println!(" - Item: {}", name);
                    } else {
                        println!(" - Unnamed item: {:?}", def_id);
                    }
                    // Print additional debugging information about the item
                    let item_kind = tcx.def_kind(def_id);
                    println!("   - DefKind: {:?}", item_kind);
                    let span = tcx.def_span(def_id);
                    println!("   - Span: {:?}", span);
                }
            }
        }
    }
    */

    fn find_def_id_by_name<'tcx>(&self, tcx: TyCtxt<'tcx>, target_name: &str) -> Option<DefId> {
        for &cnum in tcx.crates(()).iter() {
            let crate_name = tcx.crate_name(cnum);
            if crate_name.as_str() == "runtime" {
                println!("Searching for '{}' in runtime crate:", target_name);
                let items = tcx.exported_non_generic_symbols(cnum);
                println!("Found {} items", items.len());
                for (symbol, _) in items {
                    match symbol {
                        ExportedSymbol::NonGeneric(def_id) | ExportedSymbol::Generic(def_id, _) => {
                            if let Some(name) = tcx.opt_item_name(*def_id) {
                                println!(" - Checking item: {}", name);
                                if name.as_str() == target_name {
                                    println!(" - Match found for '{}'", target_name);
                                    return Some(*def_id);
                                }
                            } else {
                                println!(" - Unnamed item: {:?}", def_id);
                            }
                        }
                        ExportedSymbol::NoDefId(symbol_name) => {
                            println!(" - Symbol without DefId: {:?}", symbol_name);
                        }
                        ExportedSymbol::DropGlue(ty) => {
                            println!(" - DropGlue for type: {:?}", ty);
                        }
                        ExportedSymbol::AsyncDropGlueCtorShim(ty) => {
                            println!(" - AsyncDropGlueCtorShim for type: {:?}", ty);
                        }
                        ExportedSymbol::AsyncDropGlue(def_id, ty) => {
                            println!(" - AsyncDropGlue for DefId: {:?}, type: {:?}", def_id, ty);
                        }
                        ExportedSymbol::ThreadLocalShim(def_id) => {
                            println!(" - ThreadLocalShim for DefId: {:?}", def_id);
                        }
                        _ => {
                            println!(" - Unhandled ExportedSymbol variant");
                        }
                    }
                }
            }
        }
        println!("No match found for '{}'", target_name);
        None
    }

    fn run_pass<'tcx>(&self, tcx: TyCtxt<'tcx>, body: &mut Body<'tcx>) {
        let def_id = body.source.def_id();
        let def_path = tcx.def_path_str(def_id);

        if def_path.contains("runtime") {
            println!("Skipping optimization for {}", def_path);
            return;
        }

        let crate_name = tcx.crate_name(def_id.krate);

        // Skip the `runtime` crate
        if crate_name.as_str() == "runtime" {
            println!(
                "Skipping optimization for item in runtime crate: {:?}",
                def_id
            );
            return;
        }

        println!(
            "Running MyOptimizationPass on {:?} {:?}",
            body.source.def_id(),
            def_path
        );

        println!("Loaded crates:");
        for &cnum in tcx.crates(()).iter() {
            let name = tcx.crate_name(cnum);
            println!(" - {:?} (cnum: {:?})", name, cnum);
        }

        // self.print_runtime_items(tcx);

        let def_id_ref = self
            .find_def_id_by_name(tcx, "__record_ref_creation")
            .expect("missing '__record_ref_creation' definition");
        let def_id_raw = self
            .find_def_id_by_name(tcx, "__record_raw_ptr_creation")
            .expect("missing '__record_raw_ptr_creation' definition");
        let def_id_alloc = self
            .find_def_id_by_name(tcx, "__rz_record_alloc")
            .expect("missing '__rz_record_alloc' definition");
        let def_id_write = self
            .find_def_id_by_name(tcx, "__rz_ptr_write")
            .expect("missing '__rz_ptr_write' definition");
        let def_id_read = self
            .find_def_id_by_name(tcx, "__rz_ptr_read")
            .expect("missing '__rz_ptr_read' definition");
        let def_id_use = self
            .find_def_id_by_name(tcx, "__rz_ptr_use")
            .expect("missing '__rz_ptr_use' definition");

        let func_operand_ref =
            move |sp: Span| Operand::function_handle(tcx, def_id_ref, std::iter::empty(), sp);
        let func_operand_raw =
            move |sp: Span| Operand::function_handle(tcx, def_id_raw, std::iter::empty(), sp);
        let func_operand_alloc =
            move |sp: Span| Operand::function_handle(tcx, def_id_alloc, std::iter::empty(), sp);
        let func_operand_write =
            move |sp: Span| Operand::function_handle(tcx, def_id_write, std::iter::empty(), sp);
        let func_operand_read =
            move |sp: Span| Operand::function_handle(tcx, def_id_read, std::iter::empty(), sp);
        let func_operand_use =
            move |sp: Span| Operand::function_handle(tcx, def_id_use, std::iter::empty(), sp);

        // We'll collect all insertion points first to avoid borrow issues.
        #[derive(Copy, Clone, Debug)]
        enum InstrKind<'tcx> {
            Ref { bk: BorrowKind, src: Place<'tcx> },
            Raw { is_mut: bool, src: Place<'tcx> },
            /// Stack allocation lifetime event for a MIR local.
            StackAlloc { local: Local, live: bool, size: usize },
            /// A write through a pointer local.
            /// `size` is best-effort (0 = unknown).
            PtrWrite { ptr_local: Local, size: usize },
            /// A read through a pointer local.
            /// `size` is best-effort (0 = unknown).
            PtrRead { ptr_local: Local, size: usize },
            /// Coarse pointer-use telemetry: a pointer-typed local appears in a call argument.
            PtrUse { ptr_local: Local },
            /// Propagate tags across pointer-to-pointer casts and plain copies/moves of pointer locals.
            /// This is a local tag assignment, not a runtime hook.
            TagProp { dst: Local, src: Local },
        }

        let mut insert_points: Vec<(BasicBlock, usize, SourceInfo, Place<'tcx>, InstrKind<'tcx>)> = Vec::new();

        // Stable mapping: for each pointer local (e.g., `_2`), allocate exactly one u64 local to hold its tag.
        // This avoids ordering issues when inserting instrumentation in reverse order.
        let mut tag_local_for_ptr_local: HashMap<Local, Local> = HashMap::new();

        // During the scan we must not mutate `body` while iterating basic blocks.
        // Collect pointer locals first, then allocate their tag locals afterwards.
        let mut ptr_locals_needing_tag: HashSet<Local> = HashSet::new();

        for (bb, block_data) in body.basic_blocks.iter_enumerated() {
            for (stmt_idx, stmt) in block_data.statements.iter().enumerate() {
                // Stack allocation lifetime: StorageLive/StorageDead.
                match stmt.kind {
                    StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
                        if local != RETURN_PLACE {
                            let live = matches!(stmt.kind, StatementKind::StorageLive(_));
                            let ty = body.local_decls[local].ty;

                            // On nightly-2025-08-01, `tcx.layout_of` expects a `PseudoCanonicalInput<Ty>`.
                            // For MIR locals, `TypingEnv::fully_monomorphized()` is sufficient.
                            let size = {
                                let input = PseudoCanonicalInput {
                                    typing_env: TypingEnv::fully_monomorphized(),
                                    value: ty,
                                };
                                tcx.layout_of(input)
                                    .ok()
                                    .map(|l| l.size.bytes() as usize)
                                    .unwrap_or(0)
                            };

                            // insert_points.push((
                            //     bb,
                            //     stmt_idx,
                            //     stmt.source_info,
                            //     Place::from(local),
                            //     InstrKind::StackAlloc { local, live, size },
                            // ));
                        }
                    }
                    _ => {}
                }
                // Pointer write: any assignment whose LHS place begins with a Deref projection.
                // NOTE: We intentionally operate on *optimized MIR*. This means some semantic pointer writes
                // like `*p = v` can be optimized into plain local assignments (e.g., `_x = v`) and will not appear
                // as an LHS `Deref` store anymore. 
                // TODO: Address precision: `PtrWrite` currently reports `addr = expose_provenance(ptr_local)`
                // (the pointer value). This is fine for `*p = ...`, but for interior stores like `(*p).field = ...`
                // or indexing, the true store address is `base + offset` from projections after `Deref`. We should
                // eventually compute and pass the real accessed address.
                if let StatementKind::Assign(box (lhs_place, _rhs)) = &stmt.kind {
                    let is_deref_write = lhs_place
                        .projection
                        .iter()
                        .next()
                        .is_some_and(|pe| matches!(pe, ProjectionElem::Deref));
                    if is_deref_write {
                        let ptr_local = lhs_place.local;
                        // Use size=0 for unknown size in deref writes.
                        insert_points.push((
                            bb,
                            stmt_idx,
                            stmt.source_info,
                            Place::from(ptr_local),
                            InstrKind::PtrWrite { ptr_local, size: 0 },
                        ));
                    }
                }
                // Tag propagation: handle `_dst = copy/move _src` and `_dst = (copy/move _src) as *const/*mut U (PtrToPtr)`.
                // This ensures derived pointer locals keep a non-zero tag.
                if let StatementKind::Assign(box (dst_place, rvalue)) = &stmt.kind {
                    if let Some(dst_local) = dst_place.as_local() {
                        let dst_ty = body.local_decls[dst_local].ty;
                        let dst_is_ptr = matches!(dst_ty.kind(), TyKind::Ref(..) | TyKind::RawPtr(..));
                        if dst_is_ptr {
                            // Extract a source local if the rvalue is a plain use or a PtrToPtr cast.
                            let src_local_opt: Option<Local> = match rvalue {
                                Rvalue::Use(op) => match op {
                                    Operand::Copy(p) | Operand::Move(p) => p.as_local(),
                                    _ => None,
                                },
                                Rvalue::Cast(CastKind::PtrToPtr, op, _to_ty) => match op {
                                    Operand::Copy(p) | Operand::Move(p) => p.as_local(),
                                    _ => None,
                                },
                                _ => None,
                            };

                            if let Some(src_local) = src_local_opt {
                                let src_ty = body.local_decls[src_local].ty;
                                let src_is_ptr = matches!(src_ty.kind(), TyKind::Ref(..) | TyKind::RawPtr(..));
                                if src_is_ptr {
                                    // Ensure both locals have tag locals allocated.
                                    ptr_locals_needing_tag.insert(dst_local);
                                    ptr_locals_needing_tag.insert(src_local);

                                    insert_points.push((
                                        bb,
                                        stmt_idx,
                                        stmt.source_info,
                                        Place::from(dst_local),
                                        InstrKind::TagProp {
                                            dst: dst_local,
                                            src: src_local,
                                        },
                                    ));
                                }
                            }
                        }
                    }
                }
                if let StatementKind::Assign(box (place, Rvalue::Ref(_, bk, src_place))) = &stmt.kind {
                    if let Some(lhs_local) = place.as_local() {
                        ptr_locals_needing_tag.insert(lhs_local);
                    }
                    insert_points.push((
                        bb,
                        stmt_idx,
                        stmt.source_info,
                        place.clone(),
                        InstrKind::Ref { bk: *bk, src: src_place.clone() },
                    ));
                    println!(
                        "Found ref creation at block {:?}, stmt idx {}: {:?}",
                        bb, stmt_idx, stmt
                    );
                }

                // Raw pointer creation: e.g., `_3 = &raw const _1;` or `_3 = &raw mut _1;`
                if let StatementKind::Assign(box (place, Rvalue::RawPtr(mutbl, src_place))) =
                    &stmt.kind
                {
                    if let Some(lhs_local) = place.as_local() {
                        ptr_locals_needing_tag.insert(lhs_local);
                    }
                    let is_mut = matches!(*mutbl, RawPtrKind::Mut);
                    insert_points.push((
                        bb,
                        stmt_idx,
                        stmt.source_info,
                        place.clone(),
                        InstrKind::Raw { is_mut, src: src_place.clone() },
                    ));
                    println!(
                        "Found raw pointer creation at block {:?}, stmt idx {}: {:?} = &raw {:?} {:?}",
                        bb, stmt_idx, stmt, mutbl, src_place
                    );
                }
            }
            // Calls: in optimized MIR, certain writes appear only as intrinsic calls (e.g., volatile_store).
            // We classify those as writes, and we also emit coarse PtrUse telemetry for any pointer args.
            if let Some(term) = &block_data.terminator {
                if let TerminatorKind::Call { func, args, .. } = &term.kind {
                    // Detect `std::intrinsics::volatile_{store,load}::<T>` and classify as write/read.
                    let mut is_volatile_store = false;
                    let mut is_volatile_load = false;
                    let mut classified_write_ptr_local: Option<Local> = None;
                    let mut classified_read_ptr_local: Option<Local> = None;
                    if let TyKind::FnDef(callee_def_id, _) = func.ty(body, tcx).kind() {
                        let path = tcx.def_path_str(*callee_def_id);
                        if path.contains("intrinsics::volatile_store") {
                            is_volatile_store = true;
                        }
                        if path.contains("intrinsics::volatile_load") {
                            is_volatile_load = true;
                        }
                    }

                    if is_volatile_store {
                        // Arg0 is the destination pointer.
                        if let Some(first) = args.get(0) {
                            let op0 = &first.node;
                            let pl0: Option<Place<'tcx>> = match op0 {
                                Operand::Copy(p) | Operand::Move(p) => Some(*p),
                                _ => None,
                            };
                            if let Some(p0) = pl0 {
                                classified_write_ptr_local = Some(p0.local);
                                // Best-effort size_of::<T> from the raw pointer type *const/*mut T.
                                let mut size = 0usize;
                                let ty0 = body.local_decls[p0.local].ty;
                                if let TyKind::RawPtr(pointee_ty, _mutbl) = ty0.kind() {
                                    let input = PseudoCanonicalInput {
                                        typing_env: TypingEnv::fully_monomorphized(),
                                        value: *pointee_ty,
                                    };
                                    size = tcx
                                        .layout_of(input)
                                        .ok()
                                        .map(|l| l.size.bytes() as usize)
                                        .unwrap_or(0);
                                }

                                insert_points.push((
                                    bb,
                                    block_data.statements.len(), // insert right before terminator
                                    term.source_info,
                                    Place::from(p0.local),
                                    InstrKind::PtrWrite {
                                        ptr_local: p0.local,
                                        size,
                                    },
                                ));
                            }
                        }
                    }

                    if is_volatile_load {
                        // Arg0 is the source pointer.
                        if let Some(first) = args.get(0) {
                            let op0 = &first.node;
                            let pl0: Option<Place<'tcx>> = match op0 {
                                Operand::Copy(p) | Operand::Move(p) => Some(*p),
                                _ => None,
                            };
                            if let Some(p0) = pl0 {
                                classified_read_ptr_local = Some(p0.local);
                                // Best-effort size_of::<T> from the raw pointer type *const/*mut T.
                                let mut size = 0usize;
                                let ty0 = body.local_decls[p0.local].ty;
                                if let TyKind::RawPtr(pointee_ty, _mutbl) = ty0.kind() {
                                    let input = PseudoCanonicalInput {
                                        typing_env: TypingEnv::fully_monomorphized(),
                                        value: *pointee_ty,
                                    };
                                    size = tcx
                                        .layout_of(input)
                                        .ok()
                                        .map(|l| l.size.bytes() as usize)
                                        .unwrap_or(0);
                                }

                                insert_points.push((
                                    bb,
                                    block_data.statements.len(), // insert right before terminator
                                    term.source_info,
                                    Place::from(p0.local),
                                    InstrKind::PtrRead {
                                        ptr_local: p0.local,
                                        size,
                                    },
                                ));
                            }
                        }
                    }

                    // Coarse pointer-use telemetry for any pointer-typed locals in call arguments.
                    for a in args.iter() {
                        let op = &a.node;
                        let pl: Option<Place<'tcx>> = match op {
                            Operand::Copy(p) | Operand::Move(p) => Some(*p),
                            _ => None,
                        };
                        if let Some(p) = pl {
                            let ty = body.local_decls[p.local].ty;
                            let is_ptr = matches!(ty.kind(), TyKind::Ref(..) | TyKind::RawPtr(..));
                            if is_ptr {
                                // Avoid double-reporting: if this call site was classified as a write or read on arg0,
                                // do not also emit the coarse PtrUse for the same pointer local.
                                if classified_write_ptr_local == Some(p.local)
                                    || classified_read_ptr_local == Some(p.local)
                                {
                                    continue;
                                }

                                insert_points.push((
                                    bb,
                                    block_data.statements.len(), // insert right before terminator
                                    term.source_info,
                                    Place::from(p.local),
                                    InstrKind::PtrUse { ptr_local: p.local },
                                ));
                            }
                        }
                    }
                }
            }
        }

        // Allocate one stable tag local per pointer local we detected as being created (Ref/Raw).
        // Do this after scanning to avoid borrowing `body` mutably while iterating basic blocks.
        for ptr_local in ptr_locals_needing_tag.drain() {
            if !tag_local_for_ptr_local.contains_key(&ptr_local) {
                let t = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, rustc_span::DUMMY_SP));
                tag_local_for_ptr_local.insert(ptr_local, t);
            }
        }

        // Insert in reverse order to not invalidate indices
        for (bb, stmt_idx, source_info, place, creation_kind) in insert_points.into_iter().rev() {
            // Tag propagation is a local assignment (no runtime call). Insert it and continue.
            if let InstrKind::TagProp { dst, src } = creation_kind {
                println!(
                    "[instrument-mir] TAG PROPAGATION: dst_local={:?} src_local={:?}",
                    dst,
                    src
                );
                let dst_tag = *tag_local_for_ptr_local
                    .get(&dst)
                    .expect("missing tag local for TagProp dst");

                let src_op: Operand<'tcx> = if let Some(src_tag) = tag_local_for_ptr_local.get(&src) {
                    Operand::Copy(Place::from(*src_tag))
                } else {
                    Operand::Constant(Box::new(ConstOperand {
                        span: source_info.span,
                        user_ty: None,
                        const_: Const::Val(
                            ConstValue::Scalar(Scalar::from_u64(0)),
                            tcx.types.u64,
                        ),
                    }))
                };

                let prop_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(dst_tag),
                        Rvalue::Use(src_op),
                    ))),
                );

                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                let insert_at = if stmt_idx >= bd.statements.len() {
                    bd.statements.len()
                } else {
                    stmt_idx + 1
                };
                bd.statements.insert(insert_at, prop_stmt);
                continue;
            }
            let (orig_term, is_cleanup) = {
                let bd = &mut body.basic_blocks_mut()[bb];
                let term = bd.terminator.take();
                let cleanup = bd.is_cleanup;
                (term, cleanup)
            };

            println!("Terminator at block {:?}: {:?}", bb, orig_term);

            // Build continuation block now (no outstanding borrow of `bb`)
            let cont_block = {
                let cont_data = BasicBlockData::new(orig_term, is_cleanup);
                body.basic_blocks_mut().push(cont_data)
            };

            // Build function operand and tag destination local (no outstanding borrow of `bb`)
            let func_operand = match creation_kind {
                InstrKind::Ref { .. } => func_operand_ref(source_info.span),
                InstrKind::Raw { .. } => func_operand_raw(source_info.span),
                InstrKind::StackAlloc { .. } => func_operand_alloc(source_info.span),
                InstrKind::PtrWrite { .. } => func_operand_write(source_info.span),
                InstrKind::PtrRead { .. } => func_operand_read(source_info.span),
                InstrKind::PtrUse { .. } => func_operand_use(source_info.span),
                InstrKind::TagProp { .. } => unreachable!("TagProp is handled earlier via continue"),
            };

            // For Ref/Raw creation we write the returned tag into the preallocated tag-local for the destination.
            // For other instrumentation kinds we do not create/update tags here.
            let tag_local: Option<Local> = match creation_kind {
                InstrKind::Ref { .. } | InstrKind::Raw { .. } => {
                    if let Some(lhs_local) = place.as_local() {
                        Some(
                            *tag_local_for_ptr_local
                                .get(&lhs_local)
                                .expect("missing preallocated tag local for pointer destination"),
                        )
                    } else {
                        None
                    }
                }
                _ => None,
            };

            // Compute the address that the newly-created reference points to.
            // For StackAlloc we need the address of the local's storage slot, not the pointee address.
            // We compute: tmp_ptr = &raw const <local>; addr = expose_provenance(tmp_ptr).
            let addr_local = body
                .local_decls
                .push(LocalDecl::new(tcx.types.usize, source_info.span));

            let (addr_stmt1_opt, addr_stmt2) = match creation_kind {
                InstrKind::StackAlloc { local, .. } => {
                    let local_ty = body.local_decls[local].ty;
                    let ptr_ty = Ty::new_imm_ptr(tcx, local_ty);
                    let tmp_ptr = body
                        .local_decls
                        .push(LocalDecl::new(ptr_ty, source_info.span));

                    let s1 = Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(tmp_ptr),
                            Rvalue::RawPtr(RawPtrKind::Const, Place::from(local)),
                        ))),
                    );

                    let s2 = Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(addr_local),
                            Rvalue::Cast(
                                CastKind::PointerExposeProvenance,
                                Operand::Copy(Place::from(tmp_ptr)),
                                tcx.types.usize,
                            ),
                        ))),
                    );

                    (Some(s1), s2)
                }
                _ => {
                    let s2 = Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(addr_local),
                            Rvalue::Cast(
                                CastKind::PointerExposeProvenance,
                                Operand::Copy(place),
                                tcx.types.usize,
                            ),
                        ))),
                    );
                    (None, s2)
                }
            };

            let arg_addr = Operand::Copy(Place::from(addr_local));

            let (args, dest_place) = match creation_kind {
                InstrKind::PtrRead { ptr_local, size } => {
                    // __rz_ptr_read(tag, addr, size) -> ()
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let tag_op: Operand<'tcx> = if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local) {
                        Operand::Copy(Place::from(*tl))
                    } else {
                        Operand::Constant(Box::new(ConstOperand {
                            span: source_info.span,
                            user_ty: None,
                            const_: Const::Val(
                                ConstValue::Scalar(Scalar::from_u64(0)),
                                tcx.types.u64,
                            ),
                        }))
                    };

                    let arg_size0 = Operand::Constant(Box::new(ConstOperand {
                        span: source_info.span,
                        user_ty: None,
                        const_: Const::Val(
                            ConstValue::Scalar(Scalar::from_u64(size as u64)),
                            tcx.types.usize,
                        ),
                    }));

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned { node: tag_op, span: source_info.span },
                        Spanned { node: arg_addr, span: source_info.span },
                        Spanned { node: arg_size0, span: source_info.span },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }
                InstrKind::StackAlloc { size, live, .. } => {
                    // __rz_record_alloc(base_addr, size, live) -> ()
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let arg_size = Operand::Constant(Box::new(ConstOperand {
                        span: source_info.span,
                        user_ty: None,
                        const_: Const::Val(
                            ConstValue::Scalar(Scalar::from_u64(size as u64)),
                            tcx.types.usize,
                        ),
                    }));

                    let arg_live = Operand::Constant(Box::new(ConstOperand {
                        span: source_info.span,
                        user_ty: None,
                        const_: Const::Val(
                            ConstValue::Scalar(Scalar::from_u8(if live { 1 } else { 0 })),
                            tcx.types.u8,
                        ),
                    }));

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned { node: arg_addr, span: source_info.span },
                        Spanned { node: arg_size, span: source_info.span },
                        Spanned { node: arg_live, span: source_info.span },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }
                InstrKind::PtrWrite { ptr_local, size } => {
                    // __rz_ptr_write(tag, addr, size) -> ()
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let tag_op: Operand<'tcx> = if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local) {
                        Operand::Copy(Place::from(*tl))
                    } else {
                        Operand::Constant(Box::new(ConstOperand {
                            span: source_info.span,
                            user_ty: None,
                            const_: Const::Val(
                                ConstValue::Scalar(Scalar::from_u64(0)),
                                tcx.types.u64,
                            ),
                        }))
                    };

                    let arg_size0 = Operand::Constant(Box::new(ConstOperand {
                        span: source_info.span,
                        user_ty: None,
                        const_: Const::Val(
                            ConstValue::Scalar(Scalar::from_u64(size as u64)),
                            tcx.types.usize,
                        ),
                    }));

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned { node: tag_op, span: source_info.span },
                        Spanned { node: arg_addr, span: source_info.span },
                        Spanned { node: arg_size0, span: source_info.span },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::PtrUse { ptr_local } => {
                    // __rz_ptr_use(tag, addr) -> ()
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let tag_op: Operand<'tcx> = if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local) {
                        Operand::Copy(Place::from(*tl))
                    } else {
                        Operand::Constant(Box::new(ConstOperand {
                            span: source_info.span,
                            user_ty: None,
                            const_: Const::Val(
                                ConstValue::Scalar(Scalar::from_u64(0)),
                                tcx.types.u64,
                            ),
                        }))
                    };

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned { node: tag_op, span: source_info.span },
                        Spanned { node: arg_addr, span: source_info.span },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }
                _ => {
                    // Ref/Raw creation: (pointee_addr, is_mut, parent_tag) -> tag
                    let is_mut_u8: u8 = match creation_kind {
                        InstrKind::Ref { bk: borrow_kind, .. } => match borrow_kind {
                            BorrowKind::Mut { .. } => 1,
                            _ => 0,
                        },
                        InstrKind::Raw { is_mut, .. } => if is_mut { 1 } else { 0 },
                        InstrKind::StackAlloc { .. } => 0,
                        InstrKind::PtrWrite { .. } => 0,
                        InstrKind::PtrRead { .. } => 0,
                        InstrKind::PtrUse { .. } => 0,
                        InstrKind::TagProp { .. } => 0,
                    };

                    let arg_mut = Operand::Constant(Box::new(ConstOperand {
                        span: source_info.span,
                        user_ty: None,
                        const_: Const::Val(
                            ConstValue::Scalar(Scalar::from_u8(is_mut_u8)),
                            tcx.types.u8,
                        ),
                    }));

                    // Best-effort parent tag: use the base local's tag if known, else 0.
                    let arg_parent: Operand<'tcx> = match &creation_kind {
                        InstrKind::Ref { src, .. } | InstrKind::Raw { src, .. } => {
                            let base = src.local;
                            if let Some(tl) = tag_local_for_ptr_local.get(&base) {
                                Operand::Copy(Place::from(*tl))
                            } else {
                                Operand::Constant(Box::new(ConstOperand {
                                    span: source_info.span,
                                    user_ty: None,
                                    const_: Const::Val(
                                        ConstValue::Scalar(Scalar::from_u64(0)),
                                        tcx.types.u64,
                                    ),
                                }))
                            }
                        }
                        InstrKind::StackAlloc { .. }
                        | InstrKind::PtrWrite { .. }
                        | InstrKind::PtrRead { .. }
                        | InstrKind::PtrUse { .. }
                        | InstrKind::TagProp { .. } => Operand::Constant(Box::new(ConstOperand {
                            span: source_info.span,
                            user_ty: None,
                            const_: Const::Val(
                                ConstValue::Scalar(Scalar::from_u64(0)),
                                tcx.types.u64,
                            ),
                        })),
                    };

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned { node: arg_addr, span: source_info.span },
                        Spanned { node: arg_mut, span: source_info.span },
                        Spanned { node: arg_parent, span: source_info.span },
                    ]
                    .into_boxed_slice();

                    let dst = tag_local.expect("missing tag_local for ref/raw creation");
                    (args, Place::from(dst))
                }
            };

            // Build the call terminator
            let call_term = Terminator {
                source_info,
                kind: TerminatorKind::Call {
                    func: func_operand,
                    args,
                    destination: dest_place,
                    target: Some(cont_block),
                    unwind: UnwindAction::Continue,
                    call_source: CallSource::Misc,
                    fn_span: source_info.span,
                },
            };

            // Split off remaining statements and set the block terminator in one borrow
            let remaining_stmts = {
                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];

                // If `stmt_idx` points *past the last statement* (i.e., insertion right before the terminator),
                // we must split at `stmt_idx` rather than `stmt_idx + 1`.
                let split_at = if stmt_idx >= bd.statements.len() {
                    stmt_idx
                } else {
                    stmt_idx + 1
                };

                let rem = bd.statements.split_off(split_at);

                // Insert the address-computation statement(s) at the insertion point.
                if let Some(s1) = addr_stmt1_opt {
                    bd.statements.push(s1);
                }
                bd.statements.push(addr_stmt2);

                // Then replace the terminator with our call.
                bd.terminator = Some(call_term);
                rem
            };

            // Extend the continuation block with the remaining statements
            body.basic_blocks_mut()[cont_block]
                .statements
                .extend(remaining_stmts);
        }

        // println!("{:#?}", body);
    }
}

const CUSTOM_OPT_MIR: for<'tcx> fn(tcx: TyCtxt<'tcx>, def: LocalDefId) -> &'tcx Body<'tcx> =
    |tcx, def| {
        let mut body = (rustc_interface::DEFAULT_QUERY_PROVIDERS.optimized_mir)(tcx, def).clone();

        // Write MIR before running our optimization/instrumentation.
        if let Some(path) = MIR_OUT_BEFORE.get() {
            let mut extra = |_, _: &mut dyn std::io::Write| Ok(());
            let file = File::create(path).unwrap();
            let mut writer = BufWriter::new(file);
            write_mir_fn(
                tcx,
                &body,
                &mut extra,
                &mut writer,
                rustc_middle::mir::pretty::PrettyPrintMirOptions::from_cli(tcx),
            )
            .unwrap();
            writer.flush().unwrap();
        }

        let optimization_pass = MyOptimizationPass;
        optimization_pass.run_pass(tcx, &mut body);

        // Write MIR after running our optimization/instrumentation.
        if let Some(path) = MIR_OUT_AFTER.get() {
            let mut extra = |_, _: &mut dyn std::io::Write| Ok(());
            let file = File::create(path).unwrap();
            let mut writer = BufWriter::new(file);
            write_mir_fn(
                tcx,
                &body,
                &mut extra,
                &mut writer,
                rustc_middle::mir::pretty::PrettyPrintMirOptions::from_cli(tcx),
            )
            .unwrap();
            writer.flush().unwrap();
        }

        tcx.arena.alloc(body)
    };

struct CompilerCallbacks;

impl rustc_driver::Callbacks for CompilerCallbacks {
    fn config(&mut self, _config: &mut Config) {
        _config.override_queries = Some(|_session, queries| {
            queries.optimized_mir = CUSTOM_OPT_MIR;
        });
    }
}

fn main() {
    let mut callbacks = CompilerCallbacks {};

    let handler = EarlyDiagCtxt::new(ErrorOutputType::HumanReadable {
        kind: HumanReadableErrorType::Default,
        color_config: ColorConfig::Auto,
    });
    rustc_driver::init_rustc_env_logger(&handler);
    std::process::exit(rustc_driver::catch_with_exit_code(move || {
        let mut args: Vec<String> = std::env::args().collect();

        let mut mir_out: Option<String> = None;

        args.retain(|arg| {
            if let Some(rest) = arg.strip_prefix("--mir-out=") {
                mir_out = Some(rest.to_string());
                false
            } else {
                true
            }
        });

        let mut runtime_path: Option<String> = None;

        args.retain(|arg| {
            if let Some(rest) = arg.strip_prefix("--runtime-path=") {
                runtime_path = Some(rest.to_string());
                false
            } else {
                true
            }
        });

        if let Some(p) = mir_out {
            let before = prefixed_path(&p, "before.");
            let after = prefixed_path(&p, "after.");
            MIR_OUT_BEFORE.set(before).unwrap();
            MIR_OUT_AFTER.set(after).unwrap();
        }

        // Cargo probes the compiler with `-vV` (verbose version) before building.
        // That invocation won't carry our custom flags, so we must not require them.
        let is_version_probe = args
            .iter()
            .any(|a| a == "-vV" || a == "-V" || a == "--version");

        if let Some(runtime_path) = runtime_path {
            args.push("-Zunstable-options".to_string());
            args.push(format!("-L{}", runtime_path));
            args.push(format!(
                "--extern=force:runtime={}/libruntime.rlib",
                runtime_path
            ));
        } else if !is_version_probe {
            panic!("missing --runtime-path argument (pass it via `cargo instrument-mir --runtime-path=...`)");
        }
        // args.push("-Zdump-mir=main".to_string());
        rustc_driver::run_compiler(&args, &mut callbacks)
    }))
}

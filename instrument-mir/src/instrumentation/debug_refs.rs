//! Finds source-level reference bindings and activates their debug tags.

use rustc_hir::intravisit::{self, Visitor as HirVisitor};
use rustc_hir::{self as hir, Mutability};

use super::*;

struct HirRefBindingCollector<'tcx> {
    typeck: &'tcx rustc_middle::ty::TypeckResults<'tcx>,
    bindings: Vec<HirRefBinding<'tcx>>,
}

impl<'tcx> HirVisitor<'tcx> for HirRefBindingCollector<'tcx> {
    fn visit_stmt(&mut self, stmt: &'tcx hir::Stmt<'tcx>) {
        if let hir::StmtKind::Let(let_stmt) = stmt.kind {
            let_stmt.pat.walk_always(|pat| {
                if let hir::PatKind::Binding(_, _, ident, _) = pat.kind {
                    let ty = self.typeck.pat_ty(pat);
                    if matches!(ty.kind(), TyKind::Ref(..)) {
                        self.bindings.push(HirRefBinding {
                            name: ident.name,
                            span: pat.span,
                            ty,
                        });
                    }
                }
            });
        }
        intravisit::walk_stmt(self, stmt);
    }
}
fn span_contains(outer: Span, inner: Span) -> bool {
    outer.ctxt() == inner.ctxt() && outer.lo() <= inner.lo() && inner.hi() <= outer.hi()
}

impl MyOptimizationPass {
    pub(in crate::instrumentation) fn collect_hir_ref_bindings<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
    ) -> Vec<HirRefBinding<'tcx>> {
        let Some(local_def_id) = body.source.def_id().as_local() else {
            return Vec::new();
        };
        if !tcx.hir_body_owner_kind(local_def_id).is_fn_or_closure() {
            return Vec::new();
        }
        let Some(hir_body) = tcx.hir_maybe_body_owned_by(local_def_id) else {
            return Vec::new();
        };
        let typeck = tcx.typeck(local_def_id);
        let mut collector = HirRefBindingCollector {
            typeck,
            bindings: Vec::new(),
        };
        collector.visit_body(hir_body);
        collector.bindings
    }
    pub(in crate::instrumentation) fn collect_debug_ref_bindings<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
    ) -> Vec<(DebugRefBindingKey, bool)> {
        let hir_bindings = self.collect_hir_ref_bindings(tcx, body);
        if hir_bindings.is_empty() {
            return Vec::new();
        }

        let mut out: Vec<(DebugRefBindingKey, bool)> = Vec::new();
        for info in &body.var_debug_info {
            let VarDebugInfoContents::Place(place) = info.value else {
                continue;
            };
            if !place.projection.is_empty() {
                continue;
            }
            let raw_local = place.local;
            if !matches!(body.local_decls[raw_local].ty.kind(), TyKind::RawPtr(..)) {
                continue;
            }

            let scope = info.source_info.scope;
            let scope_span = body.source_scopes[scope].span;
            let Some(binding) = hir_bindings
                .iter()
                .filter(|binding| binding.name == info.name)
                .filter(|binding| {
                    span_contains(scope_span, binding.span)
                        || span_contains(binding.span, scope_span)
                        || span_contains(info.source_info.span, binding.span)
                        || span_contains(binding.span, info.source_info.span)
                })
                .min_by_key(|binding| binding.span.hi().0 - binding.span.lo().0)
            else {
                continue;
            };

            let TyKind::Ref(_, _, mutbl) = binding.ty.kind() else {
                continue;
            };

            let key = DebugRefBindingKey { scope, raw_local };
            let is_mut = matches!(mutbl, Mutability::Mut);
            if !out.iter().any(|(existing, _)| *existing == key) {
                out.push((key, is_mut));
            }
        }

        out
    }
    pub(in crate::instrumentation) fn scope_is_within<'tcx>(
        &self,
        body: &Body<'tcx>,
        mut current: SourceScope,
        target: SourceScope,
    ) -> bool {
        loop {
            if current == target {
                return true;
            }
            let Some(parent) = body.source_scopes[current].parent_scope else {
                return false;
            };
            current = parent;
        }
    }
    pub(in crate::instrumentation) fn scope_entry_locations<'tcx>(
        &self,
        body: &Body<'tcx>,
        scope: SourceScope,
    ) -> Vec<(BasicBlock, usize, SourceInfo)> {
        let predecessors = body.basic_blocks.predecessors();
        let mut out = Vec::new();
        for (bb, data) in body.basic_blocks.iter_enumerated() {
            let mut first_loc: Option<(usize, SourceInfo)> = None;
            for (stmt_idx, stmt) in data.statements.iter().enumerate() {
                if !self.scope_is_within(body, stmt.source_info.scope, scope) {
                    continue;
                }
                first_loc = Some((stmt_idx, stmt.source_info));
                break;
            }

            if first_loc.is_none() {
                if let Some(term) = data.terminator.as_ref() {
                    if self.scope_is_within(body, term.source_info.scope, scope) {
                        first_loc = Some((data.statements.len(), term.source_info));
                    }
                }
            }

            let Some((stmt_idx, source_info)) = first_loc else {
                continue;
            };

            let enters_scope = predecessors[bb].iter().all(|pred| {
                body.basic_blocks[*pred]
                    .terminator
                    .as_ref()
                    .map(|term| !self.scope_is_within(body, term.source_info.scope, scope))
                    .unwrap_or(true)
            });
            if enters_scope {
                out.push((bb, stmt_idx, source_info));
            }
        }
        out
    }
    pub(in crate::instrumentation) fn debug_ref_activation_locations<'tcx>(
        &self,
        body: &Body<'tcx>,
        scope: SourceScope,
        raw_local: Local,
    ) -> Vec<(BasicBlock, usize, SourceInfo)> {
        let mut out = Vec::new();
        for (bb, entry_stmt_idx, entry_source_info) in self.scope_entry_locations(body, scope) {
            let block = &body.basic_blocks[bb];
            let mut activation = None;
            for (stmt_idx, stmt) in block.statements[..entry_stmt_idx].iter().enumerate().rev() {
                let StatementKind::Assign(box (dst_place, _)) = &stmt.kind else {
                    continue;
                };
                if dst_place.as_local() != Some(raw_local) {
                    continue;
                }
                activation = Some((bb, stmt_idx, stmt.source_info));
                break;
            }
            out.push(activation.unwrap_or((bb, entry_stmt_idx, entry_source_info)));
        }
        out.sort_unstable_by_key(|(bb, stmt_idx, _)| (bb.index(), *stmt_idx));
        out.dedup_by_key(|(bb, stmt_idx, _)| (bb.index(), *stmt_idx));
        out
    }
    pub(in crate::instrumentation) fn active_debug_ref_binding_tag_local<'tcx>(
        &self,
        body: &Body<'tcx>,
        scope: SourceScope,
        access_span: Span,
        raw_local: Local,
        debug_ref_bindings: &HashMap<DebugRefBindingKey, DebugRefBinding>,
    ) -> Option<Local> {
        let mut cur = Some(scope);
        while let Some(scope) = cur {
            let key = DebugRefBindingKey { scope, raw_local };
            if let Some(binding) = debug_ref_bindings.get(&key) {
                return Some(binding.tag_local);
            }
            cur = body.source_scopes[scope].parent_scope;
        }
        let mut best: Option<(u32, Local)> = None;
        for (key, binding) in debug_ref_bindings.iter() {
            if key.raw_local != raw_local {
                continue;
            }
            let binding_span = body.source_scopes[key.scope].span;
            if !span_contains(binding_span, access_span) {
                continue;
            }
            let len = binding_span.hi().0 - binding_span.lo().0;
            match best {
                Some((best_len, _)) if best_len <= len => {}
                _ => best = Some((len, binding.tag_local)),
            }
        }
        if let Some((_, tag_local)) = best {
            return Some(tag_local);
        }
        None
    }
}

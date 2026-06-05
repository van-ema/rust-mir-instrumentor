//! Checks whether types are pointers, references, wrappers, or carriers.

use super::super::*;

impl MyOptimizationPass {
    /// Return true for any pointer or reference type, including wide pointers like slices and str.
    /// We treat these as tag-carrying so that when MIR later extracts a thin data pointer, the
    /// original tag can be propagated instead of silently dropping to tag zero.
    pub(in crate::instrumentation) fn is_pointer_ty<'tcx>(&self, ty: Ty<'tcx>) -> bool {
        matches!(ty.kind(), TyKind::Ref(..) | TyKind::RawPtr(..))
    }

    pub(in crate::instrumentation) fn is_raw_pointer_ty<'tcx>(&self, ty: Ty<'tcx>) -> bool {
        matches!(ty.kind(), TyKind::RawPtr(..))
    }

    pub(in crate::instrumentation) fn is_shadowable_ptr_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        self.is_pointer_ty(ty)
            && (self.is_thin_ptr_ty(tcx, body, ty)
                || self.ptr_ty_has_precise_wide_bounds(tcx, body, ty))
    }

    /// Best-effort recursive check for whether `ty` contains any reference/raw-pointer field.
    ///
    /// This is used for non-pointer carrier values such as `Option<&T>`, tuples, or small wrapper
    /// structs so we can still add boundary validation/lineage handling even when the MIR local is
    /// not itself pointer-typed.
    pub(in crate::instrumentation) fn ty_contains_pointer_fields<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
        depth: usize,
    ) -> bool {
        if depth == 0 {
            return false;
        }
        if self.is_pointer_ty(ty) {
            return true;
        }
        match ty.kind() {
            TyKind::Tuple(field_tys) => field_tys
                .iter()
                .any(|field_ty| self.ty_contains_pointer_fields(tcx, body, field_ty, depth - 1)),
            TyKind::Adt(adt, args) => adt.variants().iter().any(|variant| {
                variant.fields.iter().any(|field| {
                    self.ty_contains_pointer_fields(tcx, body, field.ty(tcx, args), depth - 1)
                })
            }),
            TyKind::Array(elem_ty, _) | TyKind::Slice(elem_ty) => {
                self.ty_contains_pointer_fields(tcx, body, *elem_ty, depth - 1)
            }
            TyKind::Closure(_, args) => args
                .as_closure()
                .upvar_tys()
                .iter()
                .any(|field_ty| self.ty_contains_pointer_fields(tcx, body, field_ty, depth - 1)),
            TyKind::Coroutine(_, args) => rustc_middle::ty::UpvarArgs::Coroutine(args)
                .upvar_tys()
                .iter()
                .any(|field_ty| self.ty_contains_pointer_fields(tcx, body, field_ty, depth - 1)),
            TyKind::CoroutineClosure(_, args) => args
                .as_coroutine_closure()
                .upvar_tys()
                .iter()
                .any(|field_ty| self.ty_contains_pointer_fields(tcx, body, field_ty, depth - 1)),
            _ => false,
        }
    }

    pub(in crate::instrumentation) fn ty_is_direct_pointer_wrapper<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
        depth: usize,
    ) -> bool {
        if depth == 0 {
            return false;
        }
        let TyKind::Adt(adt, args) = ty.kind() else {
            return false;
        };
        let path = tcx.def_path_str(adt.did());
        let is_wrapper = path.contains("::sync::atomic::AtomicPtr")
            || path.contains("::cell::UnsafeCell")
            || path.contains("::cell::SyncUnsafeCell")
            || path.contains("::mem::MaybeUninit")
            || path.contains("::mem::ManuallyDrop")
            || path.contains("::cell::Cell");
        if !is_wrapper {
            return false;
        }
        adt.non_enum_variant().fields.iter().any(|field| {
            let field_ty = field.ty(tcx, args);
            self.is_pointer_ty(field_ty)
                || self.ty_is_direct_pointer_wrapper(tcx, body, field_ty, depth - 1)
        })
    }

    /// Shallow structural check for non-pointer values that directly store pointer data.
    ///
    /// This is used for leaf shadow transport and similar structural handling. It intentionally
    /// does *not* imply that the outer value is a borrow carrier at call boundaries; raw-owner
    /// aggregates such as `Bytes`, `BytesMut`, `Vec`, or `Box` can satisfy this check while still
    /// being ineligible for ref-style boundary validation/export.
    pub(in crate::instrumentation) fn ty_contains_direct_pointer_fields<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        if self.is_pointer_ty(ty) {
            return true;
        }
        if self.ty_is_direct_pointer_wrapper(tcx, body, ty, 3) {
            return true;
        }
        match ty.kind() {
            TyKind::Tuple(field_tys) => field_tys.iter().any(|field_ty| {
                self.is_pointer_ty(field_ty)
                    || self.ty_is_direct_pointer_wrapper(tcx, body, field_ty, 3)
            }),
            TyKind::Adt(adt, args) => adt.variants().iter().any(|variant| {
                variant.fields.iter().any(|field| {
                    let field_ty = field.ty(tcx, args);
                    self.is_pointer_ty(field_ty)
                        || self.ty_is_direct_pointer_wrapper(tcx, body, field_ty, 3)
                })
            }),
            TyKind::Array(elem_ty, _) | TyKind::Slice(elem_ty) => {
                self.is_pointer_ty(*elem_ty)
                    || self.ty_is_direct_pointer_wrapper(tcx, body, *elem_ty, 3)
            }
            TyKind::Closure(_, args) => args.as_closure().upvar_tys().iter().any(|field_ty| {
                self.is_pointer_ty(field_ty)
                    || self.ty_is_direct_pointer_wrapper(tcx, body, field_ty, 3)
            }),
            TyKind::Coroutine(_, args) => rustc_middle::ty::UpvarArgs::Coroutine(args)
                .upvar_tys()
                .iter()
                .any(|field_ty| {
                    self.is_pointer_ty(field_ty)
                        || self.ty_is_direct_pointer_wrapper(tcx, body, field_ty, 3)
                }),
            TyKind::CoroutineClosure(_, args) => args
                .as_coroutine_closure()
                .upvar_tys()
                .iter()
                .any(|field_ty| {
                    self.is_pointer_ty(field_ty)
                        || self.ty_is_direct_pointer_wrapper(tcx, body, field_ty, 3)
                }),
            _ => false,
        }
    }

    pub(in crate::instrumentation) fn ty_is_direct_ref_wrapper<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        ty: Ty<'tcx>,
        depth: usize,
    ) -> bool {
        if depth == 0 {
            return false;
        }
        let TyKind::Adt(adt, args) = ty.kind() else {
            return false;
        };
        let path = tcx.def_path_str(adt.did());
        let is_wrapper = path.contains("::cell::UnsafeCell")
            || path.contains("::cell::SyncUnsafeCell")
            || path.contains("::mem::MaybeUninit")
            || path.contains("::mem::ManuallyDrop")
            || path.contains("::cell::Cell");
        if !is_wrapper {
            return false;
        }
        adt.non_enum_variant().fields.iter().any(|field| {
            let field_ty = field.ty(tcx, args);
            matches!(field_ty.kind(), TyKind::Ref(..))
                || self.ty_is_direct_ref_wrapper(tcx, field_ty, depth - 1)
        })
    }

    /// Shallow boundary-carrier check for non-pointer values that directly carry a source-level
    /// reference. Raw-only owner/control aggregates are intentionally excluded.
    pub(in crate::instrumentation) fn ty_contains_direct_ref_fields<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        if matches!(ty.kind(), TyKind::Ref(..)) {
            return true;
        }
        match ty.kind() {
            TyKind::Tuple(field_tys) => field_tys.iter().any(|field_ty| {
                matches!(field_ty.kind(), TyKind::Ref(..))
                    || self.ty_is_direct_ref_wrapper(tcx, field_ty, 3)
            }),
            TyKind::Adt(adt, args) => adt.variants().iter().any(|variant| {
                variant.fields.iter().any(|field| {
                    let field_ty = field.ty(tcx, args);
                    matches!(field_ty.kind(), TyKind::Ref(..))
                        || self.ty_is_direct_ref_wrapper(tcx, field_ty, 3)
                })
            }),
            TyKind::Array(elem_ty, _) | TyKind::Slice(elem_ty) => {
                matches!(elem_ty.kind(), TyKind::Ref(..))
                    || self.ty_is_direct_ref_wrapper(tcx, *elem_ty, 3)
            }
            TyKind::Closure(_, args) => args.as_closure().upvar_tys().iter().any(|field_ty| {
                matches!(field_ty.kind(), TyKind::Ref(..))
                    || self.ty_is_direct_ref_wrapper(tcx, field_ty, 3)
            }),
            TyKind::Coroutine(_, args) => rustc_middle::ty::UpvarArgs::Coroutine(args)
                .upvar_tys()
                .iter()
                .any(|field_ty| {
                    matches!(field_ty.kind(), TyKind::Ref(..))
                        || self.ty_is_direct_ref_wrapper(tcx, field_ty, 3)
                }),
            TyKind::CoroutineClosure(_, args) => args
                .as_coroutine_closure()
                .upvar_tys()
                .iter()
                .any(|field_ty| {
                    matches!(field_ty.kind(), TyKind::Ref(..))
                        || self.ty_is_direct_ref_wrapper(tcx, field_ty, 3)
                }),
            _ => false,
        }
    }

    /// Recursive check for aggregates that carry source-level references anywhere inside.
    ///
    /// `Lexer { stream: LocatingSlice<&str> }` is not a direct ref carrier like
    /// `Source { input: &str }`, but it still carries `&str` leaves. Those leaves should travel as
    /// exact field shadows, not as a synthetic whole-slot raw-owner anchor.
    pub(in crate::instrumentation) fn ty_contains_ref_fields_recursive<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        ty: Ty<'tcx>,
        depth: usize,
    ) -> bool {
        if depth == 0 {
            return false;
        }
        if matches!(ty.kind(), TyKind::Ref(..)) {
            return true;
        }
        match ty.kind() {
            TyKind::Tuple(field_tys) => field_tys
                .iter()
                .any(|field_ty| self.ty_contains_ref_fields_recursive(tcx, field_ty, depth - 1)),
            TyKind::Adt(adt, args) => adt.variants().iter().any(|variant| {
                variant.fields.iter().any(|field| {
                    self.ty_contains_ref_fields_recursive(tcx, field.ty(tcx, args), depth - 1)
                })
            }),
            TyKind::Array(elem_ty, _) | TyKind::Slice(elem_ty) => {
                self.ty_contains_ref_fields_recursive(tcx, *elem_ty, depth - 1)
            }
            TyKind::Closure(_, args) => args
                .as_closure()
                .upvar_tys()
                .iter()
                .any(|field_ty| self.ty_contains_ref_fields_recursive(tcx, field_ty, depth - 1)),
            TyKind::Coroutine(_, args) => rustc_middle::ty::UpvarArgs::Coroutine(args)
                .upvar_tys()
                .iter()
                .any(|field_ty| self.ty_contains_ref_fields_recursive(tcx, field_ty, depth - 1)),
            TyKind::CoroutineClosure(_, args) => args
                .as_coroutine_closure()
                .upvar_tys()
                .iter()
                .any(|field_ty| self.ty_contains_ref_fields_recursive(tcx, field_ty, depth - 1)),
            _ => false,
        }
    }

    /// Recursive check for aggregates that store raw-pointer leaves anywhere inside.
    ///
    /// References are treated as pointer leaves, but not raw-owner leaves. For example,
    /// `Option<&[u8]>` returns false, while `Bytes { ptr: NonNull<u8>, vtable: &'static Vtable }`
    /// returns true because `NonNull` structurally contains a raw pointer.
    pub(in crate::instrumentation) fn ty_contains_raw_pointer_fields_recursive<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        ty: Ty<'tcx>,
        depth: usize,
    ) -> bool {
        if depth == 0 {
            return false;
        }
        match ty.kind() {
            TyKind::RawPtr(..) => true,
            TyKind::Ref(..) => false,
            TyKind::Tuple(field_tys) => field_tys.iter().any(|field_ty| {
                self.ty_contains_raw_pointer_fields_recursive(tcx, field_ty, depth - 1)
            }),
            TyKind::Adt(adt, args) => adt.variants().iter().any(|variant| {
                variant.fields.iter().any(|field| {
                    self.ty_contains_raw_pointer_fields_recursive(
                        tcx,
                        field.ty(tcx, args),
                        depth - 1,
                    )
                })
            }),
            TyKind::Array(elem_ty, _) | TyKind::Slice(elem_ty) => {
                self.ty_contains_raw_pointer_fields_recursive(tcx, *elem_ty, depth - 1)
            }
            TyKind::Closure(_, args) => args.as_closure().upvar_tys().iter().any(|field_ty| {
                self.ty_contains_raw_pointer_fields_recursive(tcx, field_ty, depth - 1)
            }),
            TyKind::Coroutine(_, args) => rustc_middle::ty::UpvarArgs::Coroutine(args)
                .upvar_tys()
                .iter()
                .any(|field_ty| {
                    self.ty_contains_raw_pointer_fields_recursive(tcx, field_ty, depth - 1)
                }),
            TyKind::CoroutineClosure(_, args) => args
                .as_coroutine_closure()
                .upvar_tys()
                .iter()
                .any(|field_ty| {
                    self.ty_contains_raw_pointer_fields_recursive(tcx, field_ty, depth - 1)
                }),
            _ => false,
        }
    }

    pub(in crate::instrumentation) fn compile_alias_model_is_sb_like(&self) -> bool {
        std::env::var("RZ_ALIAS_MODEL")
            .ok()
            .map(|raw| raw.to_ascii_lowercase())
            .is_some_and(|model| matches!(model.as_str(), "sb" | "sb_lite" | "stacked_borrows"))
    }

    /// Best-effort detection of "vtable-like" structs: all fields are function pointers.
    pub(in crate::instrumentation) fn is_fn_table_adt_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        let TyKind::Adt(adt, args) = ty.kind() else {
            return false;
        };
        if !adt.is_struct() {
            return false;
        }
        let variant = adt.non_enum_variant();
        if variant.fields.is_empty() {
            return false;
        }

        for field in variant.fields.iter() {
            let fty = field.ty(tcx, args);
            match fty.kind() {
                TyKind::FnPtr(..) | TyKind::FnDef(..) => {}
                _ => return false,
            }
        }
        true
    }

    /// Best-effort detection of "vtable-like" pointers: pointers to structs whose
    /// fields are all function pointers. These typically live in static memory.
    pub(in crate::instrumentation) fn is_vtable_like_ptr_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        let pointee = match ty.kind() {
            TyKind::Ref(_, p, _) | TyKind::RawPtr(p, _) => *p,
            _ => return false,
        };
        self.is_fn_table_adt_ty(tcx, pointee)
    }

    /// Return true only for *thin* pointers (one machine word).
    ///
    /// IMPORTANT: do **not** call `tcx.layout_of` / `layout_size_bytes` here.
    /// During MIR instrumentation we may see generic/projection types that cannot be
    /// normalized yet (e.g. `&[<I as Iterator>::Item; 0]` inside `SmallVec`), and forcing a
    /// layout query can surface an `E0080` "unable to determine layout ... cannot be normalized"
    /// error during compilation.
    ///
    /// Instead, classify fat pointers syntactically by looking at the pointee type:
    /// references/raw-pointers to DSTs (`[T]`, `str`, `dyn Trait`) are fat; everything else is
    /// treated as thin.
    pub(in crate::instrumentation) fn is_thin_ptr_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        match ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
                // Be conservative for unresolved/generic pointees: if we misclassify a fat pointer
                // as thin, `PointerExposeProvenance` on the pair-typed value can ICE during codegen.
                if pointee.has_param()
                    || pointee.has_infer()
                    || pointee.has_aliases()
                    || pointee.has_opaque_types()
                    || pointee.has_placeholders()
                    || pointee.has_bound_vars()
                {
                    return false;
                }

                match pointee.kind() {
                    TyKind::Slice(..) | TyKind::Str | TyKind::Dynamic(..) => false,
                    // `extern type` is unsized but uses `()` metadata, so pointers are thin.
                    TyKind::Foreign(..) => true,
                    _ => pointee.is_sized(tcx, body.typing_env(tcx)),
                }
            }
            _ => false,
        }
    }

    pub(in crate::instrumentation) fn ptr_ty_has_sized_pointee<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        match ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
                pointee.is_sized(tcx, body.typing_env(tcx))
            }
            _ => false,
        }
    }

    /// Return true when we can safely extract a concrete address from a pointer type.
    /// This is stricter than `is_thin_ptr_ty`: we also require the pointee to be sized
    /// in the current typing environment.
    pub(in crate::instrumentation) fn is_addr_exposable_ptr_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        match ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
                (self.is_thin_ptr_ty(tcx, body, ty) || self.ptr_ty_has_sized_pointee(tcx, body, ty))
                    && pointee.is_sized(tcx, body.typing_env(tcx))
            }
            _ => false,
        }
    }

    /// Return true when call-boundary return tagging is safe and useful for this pointer type.
    ///
    /// We always include thin pointers. For wide pointers, we currently include slice/str
    /// pointers (metadata is a length and we can derive byte bounds precisely), but skip
    /// `dyn Trait`/other DST metadata forms to avoid conservative false positives.
    pub(in crate::instrumentation) fn supports_call_boundary_ret_tag_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        match ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
                if self.is_addr_exposable_ptr_ty(tcx, body, ty) {
                    return true;
                }
                matches!(pointee.kind(), TyKind::Slice(..) | TyKind::Str)
            }
            _ => false,
        }
    }

    /// Produce a thin raw pointer type suitable for extracting the data pointer from a wide pointer.
    /// We only care about the address, so a pointer to unit keeps the correct size and mutability
    /// while discarding the metadata.
    pub(in crate::instrumentation) fn data_ptr_ty_for_ptr<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        ptr_ty: Ty<'tcx>,
    ) -> Option<Ty<'tcx>> {
        match ptr_ty.kind() {
            TyKind::Ref(_, _ty, mutbl) | TyKind::RawPtr(_ty, mutbl) => {
                let is_mut = matches!(mutbl, Mutability::Mut);
                Some(if is_mut {
                    Ty::new_mut_ptr(tcx, tcx.types.unit)
                } else {
                    Ty::new_imm_ptr(tcx, tcx.types.unit)
                })
            }
            _ => None,
        }
    }

    /// Extract mutability from a raw pointer or reference type.
    pub(in crate::instrumentation) fn ptr_is_mut<'tcx>(&self, ty: Ty<'tcx>) -> bool {
        match ty.kind() {
            TyKind::Ref(_, _ty, mutbl) => matches!(mutbl, Mutability::Mut),
            TyKind::RawPtr(_ty, mutbl) => matches!(mutbl, Mutability::Mut),
            _ => false,
        }
    }
}

use super::*;

#[derive(Clone, Debug)]
pub(in crate::instrumentation) enum SizeOperand<'tcx> {
    Const(Operand<'tcx>),
    SizeOf(Ty<'tcx>),
    /// Bounds-length encoding for a sized reference pointee.
    ///
    /// Runtime-side bounds metadata distinguishes:
    /// - `usize::MAX` => unknown
    /// - `0` => precise empty wide view (`&[]`, empty `str`, ...)
    /// - `usize::MAX - 1` => exact zero-sized thin pointee (`&Cell<()>`, `&()`, ...)
    ///
    /// This variant preserves that distinction without a side-channel bit: it lowers to
    /// `size_of::<T>()`, and maps the runtime value `0` to the exact-ZST sentinel.
    RefSizedBoundsLen(Ty<'tcx>),
    AlignOf(Ty<'tcx>),
    ElemCount {
        elem_ty: Ty<'tcx>,
        count_op: Operand<'tcx>,
    },
    /// Size derived from wide-pointer metadata (slice length).
    PtrMetadataSlice {
        ptr_local: Local,
        elem_ty: Ty<'tcx>,
    },
    /// Size derived from wide-pointer metadata (str length).
    PtrMetadataStr {
        ptr_local: Local,
    },
    /// Size derived from wide-pointer metadata for a struct DST with trailing `[T]` field.
    /// Total bytes = offset_of(adt_ty, field_idx) + metadata * size_of::<elem_ty>().
    PtrMetadataAdtSlice {
        ptr_local: Local,
        adt_ty: Ty<'tcx>,
        field_idx: FieldIdx,
        elem_ty: Ty<'tcx>,
    },
}

#[derive(Clone, Debug)]
pub(in crate::instrumentation) enum InstrKind<'tcx> {
    Ref {
        bk: BorrowKind,
        src: Place<'tcx>,
        projected_reborrow_anchor_key: Option<String>,
    },
    // Raw: created by MIR Rvalue::RawPtr; can propagate a parent tag from the source place.
    // Example MIR: `_p = &raw const (*_r);` where `_r: &u8`.
    Raw {
        is_mut: bool,
        src: Place<'tcx>,
    },
    // RawRoot: synthesized for pointer values without a thin-pointer source local
    // (e.g., transmute from NonNull/Unique, projected place, const/global pointer).
    // Example MIR: `_p = transmute::<NonNull<u8>, *const u8>(_nn);`.
    /// Root raw pointer creation for a pointer value already computed in a local.
    /// std/alloc often stores pointers inside ADTs like `NonNull<T>`/`Unique<T>` and then
    /// produces a thin pointer via `Transmute`. Our TagProp only propagates between thin pointer
    /// locals, so without this the destination pointer keeps tag=0 and triggers UNKNOWN_TAG.
    RawRoot {
        ptr_local: Local,
        is_mut: bool,
        exposed_provenance: bool,
    },
    /// Stack allocation lifetime event for a MIR local.
    StackAlloc {
        local: Local,
        live: bool,
        size_op: SizeOperand<'tcx>,
    },
    /// Heap allocation lifetime event for an allocator-returned pointer.
    /// `ptr_local` holds the pointer value; `size_op` is the allocation size operand (usize).
    HeapAlloc {
        ptr_local: Local,
        live: bool,
        size_op: SizeOperand<'tcx>,
    },
    /// Global/promoted const allocation materialized as a pointer.
    /// `ptr_local` holds the pointer value; `size` is the allocation size (0 = unknown).
    /// `base_offset` is the relative offset of the pointer within the global allocation.
    ConstAlloc {
        ptr_local: Local,
        size: usize,
        base_offset: usize,
    },
    /// Global/promoted const allocation from a constant pointer operand.
    /// Used when the pointer is not stored in a local (e.g., aggregate literals).
    ConstAllocConst {
        const_op: ConstOperand<'tcx>,
        size: usize,
        base_offset: usize,
    },
    /// A write through a pointer local.
    /// `size_op` is best-effort (0 = unknown). Kept as an operand so we can pass dynamic sizes.
    PtrWrite {
        ptr_local: Local,
        size_op: SizeOperand<'tcx>,
        align_op: SizeOperand<'tcx>,
    },
    /// A write through a pointer local, but skip if the tag is uninitialized (tag=0).
    PtrWriteAllowUntagged {
        ptr_local: Local,
        size_op: SizeOperand<'tcx>,
        align_op: SizeOperand<'tcx>,
    },
    /// A write directly to a stack slot tracked via a reborrow anchor tag.
    /// Uses the allow-untagged runtime path so untouched locals do not report.
    StackSlotWriteAllowUntagged {
        local: Local,
        size_op: SizeOperand<'tcx>,
        align_op: SizeOperand<'tcx>,
    },
    /// A read through a pointer local.
    /// `size_op` is best-effort (0 = unknown). Kept as an operand so we can pass dynamic sizes.
    PtrRead {
        ptr_local: Local,
        size_op: SizeOperand<'tcx>,
        align_op: SizeOperand<'tcx>,
    },
    /// A read through a pointer local, but skip if the tag is uninitialized (tag=0).
    PtrReadAllowUntagged {
        ptr_local: Local,
        size_op: SizeOperand<'tcx>,
        align_op: SizeOperand<'tcx>,
    },
    /// Coarse pointer-use tracking: a pointer-typed local appears in a call argument.
    /// This is treated as an escape event at call boundaries.
    PtrUse {
        ptr_local: Local,
    },
    /// Restore tag metadata for a pointer local loaded from a memory slot.
    ShadowLoad {
        dst_local: Local,
        require_tag: bool,
        validate_ref: bool,
    },
    /// Store tag metadata for a pointer local into a memory slot.
    ShadowStore {
        src_local: Local,
    },
    /// Store tag metadata for a pointer local into the heap pointee slot of a `Box<T>`.
    ///
    /// This is used for calls like `Box::new(p)` where `p` is itself pointer-typed. The pointer
    /// value is written into newly-allocated heap memory owned by the returned box, so we must
    /// also write the pointer's shadow metadata into that heap slot to preserve lineage for later
    /// loads such as `let q = *boxed_ptr`.
    ShadowStoreBoxPointee {
        box_local: Local,
        src_local: Local,
    },
    /// Copy tag metadata between memory slots.
    ShadowCopySlot {
        src_place: Place<'tcx>,
    },
    /// Copy tag metadata across a byte range between memory locations.
    ShadowCopyRange {
        src_place: Place<'tcx>,
        size_op: SizeOperand<'tcx>,
    },
    /// Clear pointer-shadow metadata for a written memory range.
    ShadowKill {
        size_op: SizeOperand<'tcx>,
    },
    /// Retire the current tag carried by a pointer/ref local whose MIR lifetime ended or which is
    /// being overwritten with a new pointer value.
    TagKill {
        ptr_local: Local,
    },
    /// Retire the current tag carried by a specific hidden tag local.
    TagLocalKill {
        tag_local: Local,
    },
    /// Retain the tag currently written into a hidden tag local so it stays live while any MIR
    /// local still carries that family.
    TagRetain {
        tag_local: Local,
    },
    /// Activate a source-level reference binding that rustc optimized onto a raw local.
    /// The resulting tag is synthetic: it is stored in `tag_local` and used for accesses
    /// through the raw local while the source scope is active.
    DebugRefActivate {
        raw_local: Local,
        tag_local: Local,
        is_mut: bool,
    },
    /// Propagate tags across pointer-to-pointer casts and plain copies/moves of pointer locals.
    /// This is a local tag assignment, not a runtime hook.
    TagProp {
        dst: Local,
        src: Local,
        copy_tag: bool,
        copy_ref_ancestor: bool,
    },
    /// Propagate both tag channels from the source ref-ancestor slot.
    /// Used when a stable SSA anchor is represented by a ref local whose
    /// semantic common parent is stored in `ref_ancestor`.
    TagPropFromRefAncestor {
        dst: Local,
        src: Local,
    },
    /// Reset a non-pointer local's exact-place reborrow anchor.
    ReborrowAnchorZero {
        anchor_local: Local,
        anchor_state_local: Option<Local>,
    },
    /// Initialize a non-pointer local's exact-place reborrow anchor from the first
    /// freshly-created ref tag for that place. Later reborrows must keep the original
    /// family anchor instead of overwriting it with newer child tags.
    ReborrowAnchorSet {
        dst_local: Local,
        anchor_local: Local,
        anchor_state_local: Option<Local>,
        src_ptr_local: Local,
    },
    /// Seed a non-pointer local's exact-place reborrow anchor from a recovered lineage source.
    ReborrowAnchorSeed {
        dst_local: Local,
        src_local: Local,
        mark_slot_family: bool,
    },
    /// Snapshot a parent lineage tag for a projected/non-local source place before a call.
    /// Used when the returned pointer value should derive from a call argument place, but the
    /// target block cannot reconstruct that parent from the call-site MIR anymore.
    ParentTagSnapshot {
        dst_local: Local,
        src: Place<'tcx>,
        is_raw_creation: bool,
    },
    /// Fresh tag for a derived pointer value (pointer arithmetic like add/sub/offset).
    /// Emits either ref/raw creation based on destination kind, with `parent=tag(src)`.
    PtrDerive {
        dst: Local,
        src: Local,
        is_mut: bool,
        is_ref: bool,
        strict_validity: bool,
    },
    /// Fresh tag for a derived pointer value using an explicit pre-call parent snapshot.
    PtrDeriveParent {
        dst: Local,
        is_mut: bool,
        is_ref: bool,
        strict_validity: bool,
    },
    /// Caller-side tag push for pointer arguments to a direct call.
    CallArgPush {
        callee_id: u64,
        arg_index: u64,
        ptr_local: Local,
        parent_mode: ParentSelectionMode,
        /// Bit 0 marks the custom-MIR exact in-place source shape `Move(*ptr)`.
        flags: u8,
    },
    /// Caller-side validation for a by-value argument that is not itself pointer-typed,
    /// but carries a reference inside an aggregate/container.
    ///
    /// Examples:
    /// - `Option<&T>`
    /// - `(&T, bool)`
    /// - `struct Wrap<'a> { r: &'a T }`
    ///
    /// We do not push/take a call-boundary tag for these values because the ABI value is not a
    /// plain pointer local. Instead we recover the inner reference lineage from the carrier local
    /// and validate it immediately before the call.
    CallArgValidate {
        local: Local,
    },
    /// Caller-side: export one exact pointer-leaf shadow from a by-value raw-owner aggregate.
    ///
    /// This is structural transport, not borrow transport: the callee restores the same leaf
    /// shadow into its copied argument slot instead of synthesizing a whole-slot reference family.
    CallArgLeafPush {
        callee_id: u64,
        arg_index: u64,
        leaf_key: u64,
    },
    /// Callee-side retagging of pointer arguments from the runtime side-channel.
    ArgRetag {
        callee_id: u64,
        arg_index: u64,
        ptr_local: Local,
    },
    /// Callee-side lineage anchor initialization for a non-pointer by-value argument.
    ///
    /// Used for argument carriers such as `Option<&T>`, tuples, or small wrapper structs when the
    /// callee local is not itself pointer-typed but still needs a stable reborrow-family anchor.
    /// The callee consumes the caller-pushed call-argument tag from the runtime side channel and
    /// stores it into the local anchor slot, so later inner-ref recovery does not fall back to
    /// `parent=0`.
    ArgAnchorTake {
        callee_id: u64,
        arg_index: u64,
        local: Local,
    },
    /// Callee-side: seed a non-pointer carrier anchor from an imported pointer leaf shadow.
    ///
    /// This is the raw-owner by-value path for carriers with one structural pointer leaf. The
    /// caller transports only that leaf shadow; the callee rebuilds its local projectionless
    /// anchor from the imported leaf instead of consuming a separate whole-slot boundary tag.
    ArgAnchorSeedFromShadow {
        local: Local,
    },
    /// Callee-side: restore one exact pointer-leaf shadow into a by-value raw-owner aggregate
    /// argument slot.
    ArgLeafTake {
        callee_id: u64,
        arg_index: u64,
        leaf_key: u64,
    },
    // Callee-side validation for a return value that is not itself pointer-typed,
    /// but carries a reference inside an aggregate/container.
    ///
    /// Examples:
    /// - `Option<&T>`
    /// - `(&T, bool)`
    /// - `struct Wrap<'a> { r: &'a T }`
    ///
    /// Pointer returns use `RetPush`/`RetTake`. This hook exists for wrapper returns where the
    /// returned MIR local is not a plain pointer local, so we instead recover the inner reference
    /// lineage from `RETURN_PLACE` and validate it right before `Return`.
    RetValidate {
        callee_id: u64,
        local: Local,
    },
    /// Callee-side: export the exact shadow of one internal pointer leaf of a non-pointer return
    /// carrier right before `Return`.
    RetLeafPush {
        callee_id: u64,
        leaf_key: u64,
    },
    /// Callee-side: export the by-value carrier anchor for a non-pointer direct-ref return.
    ///
    /// This is the return-side counterpart of `RetAnchorTake`: the callee pushes the outer slot
    /// family that should become the caller-visible reborrow anchor for the returned carrier.
    RetAnchorPush {
        callee_id: u64,
        local: Local,
    },
    /// Callee-side: push the tag for a returned pointer right before `Return`.
    RetPush {
        callee_id: u64,
        ptr_local: Local,
    },
    /// Caller-side: take the pushed return tag after a call that returns a pointer.
    RetTake {
        callee_id: u64,
        dst_local: Local,
    },
    /// Caller-side: import the inner family exported by a non-pointer return carrier and seed the
    /// destination local's reborrow anchor.
    RetAnchorTake {
        callee_id: u64,
        local: Local,
    },
    /// Caller-side: restore the exact shadow for one internal pointer leaf of a non-pointer
    /// return carrier into the destination slot before control reaches the original call target.
    RetLeafTake {
        callee_id: u64,
        leaf_key: u64,
    },
    /// Caller-side: seed a fresh family for a returned owner/container carrier whose nested
    /// pointer fields are implementation detail rather than source-level borrow carriers.
    RetAnchorRoot {
        local: Local,
    },
    /// Callee-side: push the updated family for a `&mut T` carrier pointee slot on return.
    MutArgRetPush {
        callee_id: u64,
        arg_index: u64,
        ptr_local: Local,
    },
    /// Callee-side: export the exact shadow of one internal pointer leaf of a `&mut T` carrier
    /// pointee so the caller can rebuild that slot shadow after the call returns.
    MutArgRetLeafPush {
        callee_id: u64,
        arg_index: u64,
        ptr_local: Local,
        leaf_key: u64,
    },
    /// Caller-side: take the callee-exported family for a `&mut T` carrier pointee slot.
    MutArgRetTake {
        callee_id: u64,
        arg_index: u64,
        local: Local,
        ptr_local: Local,
    },
    /// Caller-side: restore one internal pointer leaf shadow for a `&mut T` carrier pointee slot
    /// before control reaches the original call target.
    MutArgRetLeafTake {
        callee_id: u64,
        arg_index: u64,
        local: Local,
        leaf_key: u64,
    },
    /// Caller-side: take the callee-exported family for a live `&mut T` local when there is no
    /// separate carrier stack slot in this frame to anchor-refresh.
    ///
    /// This covers wrappers that forward their own `&mut T` argument into a nested call. The
    /// nested callee may retag the pointee-family, and the forwarding frame must refresh the
    /// still-live `&mut` local before using or re-exporting it again.
    MutArgRetTakePtrOnly {
        callee_id: u64,
        arg_index: u64,
        ptr_local: Local,
    },
    /// Caller-side: synthesize a fresh tag for an uninstrumented call return.
    RetRoot {
        dst_local: Local,
        is_mut: bool,
        is_ref: bool,
    },
    /// Callee-side: open runtime per-activation state for this function.
    FnEnter {
        callee_id: u64,
    },
    /// Callee-side: notify runtime alias models that this function is exiting.
    FnExit {
        callee_id: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::instrumentation) enum ParentSelectionMode {
    ReceiverFamily,
    SlotFamily,
    PointeeFamily,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::instrumentation) struct PtrStateLocals {
    pub(in crate::instrumentation) tag_local: Local,
    pub(in crate::instrumentation) ref_ancestor_local: Option<Local>,
    pub(in crate::instrumentation) boundary_parent_local: Option<Local>,
    pub(in crate::instrumentation) boundary_recovered_local: Option<Local>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::instrumentation) struct CarrierSlotLocals {
    pub(in crate::instrumentation) anchor_local: Local,
    pub(in crate::instrumentation) slot_family_valid_local: Option<Local>,
}

pub(in crate::instrumentation) const CALL_ARG_FLAG_INPLACE_EXACT_SOURCE: u8 = 1;
// Boundary transport and TB-lite protector activation are separate: generic `&mut Self`
// calls still need a parent tag even when we cannot soundly attach a function protector.
pub(in crate::instrumentation) const CALL_ARG_FLAG_NO_PROTECTOR: u8 = 1 << 1;
pub(in crate::instrumentation) const CALL_ARG_BOUNDARY_ORIGIN_EXACT: u8 = 0;

#[derive(Clone, Debug)]
pub(in crate::instrumentation) struct InsertPoint<'tcx> {
    pub(in crate::instrumentation) bb: BasicBlock,
    pub(in crate::instrumentation) stmt_idx: usize,
    pub(in crate::instrumentation) insert_before: bool,
    pub(in crate::instrumentation) source_info: SourceInfo,
    pub(in crate::instrumentation) place: Place<'tcx>,
    pub(in crate::instrumentation) kind: InstrKind<'tcx>,
}

#[derive(Copy, Clone, Debug)]
pub(in crate::instrumentation) struct ShadowableLeafPtrSpec<'tcx> {
    pub(in crate::instrumentation) place: Place<'tcx>,
    pub(in crate::instrumentation) ty: Ty<'tcx>,
    pub(in crate::instrumentation) byte_offset: Option<u64>,
    pub(in crate::instrumentation) path_key: u64,
}

impl<'tcx> ShadowableLeafPtrSpec<'tcx> {
    pub(in crate::instrumentation) const PATH_KEY_MARKER: u64 = 1 << 63;

    pub(in crate::instrumentation) fn transport_key(self) -> u64 {
        self.byte_offset
            .unwrap_or(Self::PATH_KEY_MARKER | (self.path_key & !Self::PATH_KEY_MARKER))
    }
}

pub(in crate::instrumentation) const SHADOWABLE_LEAF_PTR_RECURSION_DEPTH: usize = 8;

#[derive(Clone, Debug)]
pub(in crate::instrumentation) struct ScanResult<'tcx> {
    pub(in crate::instrumentation) insert_points: Vec<InsertPoint<'tcx>>,
    pub(in crate::instrumentation) ptr_locals_needing_tag: HashSet<Local>,
    pub(in crate::instrumentation) boundary_recovered_ptr_locals: HashSet<Local>,
    pub(in crate::instrumentation) local_slot_shadow_store_locals: HashSet<Local>,
    pub(in crate::instrumentation) projected_reborrow_anchor_specs: ReborrowAnchorSpecMap,
    pub(in crate::instrumentation) projectionless_anchor_suppressed_locals: HashSet<Local>,
    pub(in crate::instrumentation) interesting_stack_locals: HashSet<Local>,
    pub(in crate::instrumentation) fallback_return_locals: Vec<(Local, SizeOperand<'tcx>)>,
    pub(in crate::instrumentation) return_sites: Vec<(BasicBlock, SourceInfo, usize)>,
}

#[derive(Clone, Debug)]
pub(in crate::instrumentation) struct HirRefBinding<'tcx> {
    pub(in crate::instrumentation) name: rustc_span::Symbol,
    pub(in crate::instrumentation) span: Span,
    pub(in crate::instrumentation) ty: Ty<'tcx>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub(in crate::instrumentation) struct DebugRefBindingKey {
    pub(in crate::instrumentation) scope: SourceScope,
    pub(in crate::instrumentation) raw_local: Local,
}

#[derive(Copy, Clone, Debug)]
pub(in crate::instrumentation) struct DebugRefBinding {
    pub(in crate::instrumentation) key: DebugRefBindingKey,
    pub(in crate::instrumentation) tag_local: Local,
    pub(in crate::instrumentation) is_mut: bool,
}

#[derive(Copy, Clone, Debug)]
pub(in crate::instrumentation) struct ConstAllocInfo {
    pub(in crate::instrumentation) size: usize,
    pub(in crate::instrumentation) base_offset: usize,
}

#[derive(Copy, Clone, Debug)]
pub(in crate::instrumentation) struct Hooks {
    pub(in crate::instrumentation) def_id_ref: DefId,
    pub(in crate::instrumentation) def_id_ref_with_extent: DefId,
    pub(in crate::instrumentation) def_id_debug_ref: DefId,
    pub(in crate::instrumentation) def_id_raw: DefId,
    pub(in crate::instrumentation) def_id_alloc: DefId,
    pub(in crate::instrumentation) def_id_write: DefId,
    pub(in crate::instrumentation) def_id_write_allow_untagged: DefId,
    pub(in crate::instrumentation) def_id_local_write_allow_untagged: DefId,
    pub(in crate::instrumentation) def_id_read: DefId,
    pub(in crate::instrumentation) def_id_read_allow_untagged: DefId,
    pub(in crate::instrumentation) def_id_use: DefId,
    pub(in crate::instrumentation) def_id_push_call_arg_tag: DefId,
    pub(in crate::instrumentation) def_id_validate_call_arg_tag: DefId,
    pub(in crate::instrumentation) def_id_take_call_arg_tag: DefId,
    pub(in crate::instrumentation) def_id_take_call_arg_tag_anchor: DefId,
    pub(in crate::instrumentation) def_id_push_call_arg_leaf_shadow: DefId,
    pub(in crate::instrumentation) def_id_take_call_arg_leaf_shadow: DefId,
    pub(in crate::instrumentation) def_id_push_ret_tag: DefId,
    pub(in crate::instrumentation) def_id_validate_ret_tag: DefId,
    pub(in crate::instrumentation) def_id_take_ret_tag: DefId,
    pub(in crate::instrumentation) def_id_push_ret_leaf_shadow: DefId,
    pub(in crate::instrumentation) def_id_take_ret_leaf_shadow: DefId,
    pub(in crate::instrumentation) def_id_validate_loaded_ref_tag: DefId,
    pub(in crate::instrumentation) def_id_require_loaded_ptr_tag: DefId,
    pub(in crate::instrumentation) def_id_take_ret_tag_or_root: DefId,
    pub(in crate::instrumentation) def_id_push_mut_arg_ret_tag: DefId,
    pub(in crate::instrumentation) def_id_take_mut_arg_ret_tag: DefId,
    pub(in crate::instrumentation) def_id_take_mut_arg_ret_tag_or_zero: DefId,
    pub(in crate::instrumentation) def_id_push_mut_arg_ret_leaf_shadow: DefId,
    pub(in crate::instrumentation) def_id_take_mut_arg_ret_leaf_shadow: DefId,
    pub(in crate::instrumentation) def_id_enter_fn: DefId,
    pub(in crate::instrumentation) def_id_exit_fn: DefId,
    pub(in crate::instrumentation) def_id_shadow_store_ptr: DefId,
    pub(in crate::instrumentation) def_id_shadow_store_ptr_local: DefId,
    pub(in crate::instrumentation) def_id_shadow_load_tag: DefId,
    pub(in crate::instrumentation) def_id_shadow_load_tag_for_ptr: DefId,
    pub(in crate::instrumentation) def_id_shadow_load_ref_ancestor: DefId,
    pub(in crate::instrumentation) def_id_shadow_load_ref_ancestor_for_ptr: DefId,
    pub(in crate::instrumentation) def_id_shadow_load_export_parent: DefId,
    pub(in crate::instrumentation) def_id_shadow_load_export_parent_for_ptr: DefId,
    pub(in crate::instrumentation) def_id_shadow_load_export_parent_recovered: DefId,
    pub(in crate::instrumentation) def_id_shadow_load_export_parent_recovered_for_ptr: DefId,
    pub(in crate::instrumentation) def_id_shadow_kill_range: DefId,
    pub(in crate::instrumentation) def_id_tag_kill: DefId,
    pub(in crate::instrumentation) def_id_tag_retain: DefId,
    pub(in crate::instrumentation) def_id_shadow_copy_slot: DefId,
    pub(in crate::instrumentation) def_id_shadow_copy_range: DefId,
}

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::{PtrKind, TagMeta};

mod tree_borrows_lite;

pub(crate) use tree_borrows_lite::TreeBorrowsLiteModel;

#[derive(Copy, Clone, Debug)]
pub(crate) enum AliasAccessKind {
    Read,
    Write,
}

pub(crate) trait AliasModel: Sync {
    fn name(&self) -> &'static str;

    fn violation_kind(&self) -> &'static str {
        "TREE_BORROWS_VIOLATION"
    }

    fn on_alloc_state_change(&self, _base_addr: usize, _new_live: bool) {}

    fn validate_ref_creation(
        &self,
        _pointee_addr: usize,
        _new_kind: PtrKind,
        _parent_tag: u64,
        _alias_exempt: bool,
        _bounds_len: usize,
    ) -> Option<String> {
        None
    }

    fn on_tag_created(&self, _tag: u64, _tmeta: &TagMeta) {}

    /// Called when MIR lifetime/overwrite semantics make an existing pointer/ref local dead.
    ///
    /// Alias models can use this to retire temporary tags so later accesses do not conflict with
    /// aliases that no longer exist in the source program.
    fn on_tag_killed(&self, _tag: u64) {}

    /// Called when a callee consumes a caller-pushed argument tag.
    /// Alias models can use this to seed call-scope metadata (e.g., protectors).
    fn on_call_arg_taken(&self, _callee_id: u64, _parent_tag: u64) {}

    /// Called when a non-pointer carrier argument (`Option<&T>`, tuple/newtype wrapper, etc.)
    /// consumes a caller-pushed inner tag.
    fn on_call_arg_anchor_taken(&self, callee_id: u64, parent_tag: u64) {
        self.on_call_arg_taken(callee_id, parent_tag);
    }

    /// Called when the call side-channel observes two arguments carrying the same tag/address.
    /// This models exact MIR shapes such as `callee(Move(*ptr), ptr)`, where Miri treats the
    /// first by-value argument as an in-place transfer from the same storage later passed by raw
    /// pointer.
    fn on_call_arg_inplace_alias(&self, _callee_id: u64, _parent_tag: u64, _addr: usize) {}

    /// Called at instrumented function return.
    fn on_call_exit(&self, _callee_id: u64) {}

    /// Validate returning a mutable reference before the caller can use it.
    ///
    /// The default keeps the old boundary-read behavior. Models with a richer borrow state can
    /// require that the exact returned `&mut` is still uniquely usable without performing a write.
    fn validate_ref_mut_boundary_retag(
        &self,
        tag: u64,
        tmeta: &TagMeta,
        addr: usize,
        size: usize,
    ) -> Option<String> {
        self.check_access(tag, tag, tmeta, addr, size, AliasAccessKind::Read)
    }

    /// Validate that a caller ref tag is a live parent for callee-entry retagging.
    ///
    /// This is narrower than a memory read: passing `&T`/`&mut T` over a call boundary transports
    /// parent authority, while the callee's retag/access hooks model the actual borrow actions.
    fn validate_call_arg_boundary_parent(
        &self,
        tag: u64,
        tmeta: &TagMeta,
        addr: usize,
        size: usize,
    ) -> Option<String> {
        self.check_access(tag, tag, tmeta, addr, size, AliasAccessKind::Read)
    }

    fn find_ref_ancestor_tag(&self, _tmap: &HashMap<u64, TagMeta>, _tag: u64) -> Option<u64> {
        None
    }

    /// Whether a tag is still a valid parent candidate for runtime lineage/call-boundary repair.
    ///
    /// Recovery paths must not resurrect alias-model-dead tags (for example a TB protected tag
    /// that was disabled at call exit), or a metadata miss turns into a false positive.
    fn can_recover_parent_tag(&self, _tag: u64) -> bool {
        true
    }

    /// Return a stable family tag suitable for exporting a mutated `&mut T` carrier back to the
    /// caller. Models that track transient child tags across a call can map those back to the
    /// live ancestor that should survive after the call boundary.
    fn canonicalize_mut_arg_ret_tag(&self, tag: u64, _addr: usize) -> u64 {
        tag
    }

    /// Mark a family exported through the mut-arg-ret side channel as surviving the current call
    /// boundary.
    ///
    /// This is the runtime backstop for cases where return-edge hook ordering still publishes the
    /// tag after call-exit teardown has run. Models may use it to revive the exported family so
    /// the caller can immediately reuse it.
    fn on_mut_arg_ret_export(&self, _tag: u64, _addr: usize) {}

    /// Mark a normal return value family as surviving the current call boundary.
    ///
    /// Like `on_mut_arg_ret_export`, this compensates for return-edge hook orderings where the
    /// callee's `FnExit` runs before the return-tag/leaf export hook.
    fn on_ret_export(&self, _tag: u64, _addr: usize) {}

    fn check_access(
        &self,
        _access_tag: u64,
        _orig_tag: u64,
        _tmeta: &TagMeta,
        _addr: usize,
        _size: usize,
        _access: AliasAccessKind,
    ) -> Option<String> {
        None
    }
}

struct NoAliasModel;

impl AliasModel for NoAliasModel {
    fn name(&self) -> &'static str {
        "none"
    }
}

static NO_ALIAS_MODEL: NoAliasModel = NoAliasModel;
static TB_LITE_MODEL: TreeBorrowsLiteModel = TreeBorrowsLiteModel;

#[derive(Copy, Clone, Debug)]
enum ActiveModel {
    TbLite,
    None,
}

fn active_model_choice() -> ActiveModel {
    static ACTIVE: OnceLock<ActiveModel> = OnceLock::new();
    *ACTIVE.get_or_init(|| {
        let raw = std::env::var("RZ_ALIAS_MODEL")
            .unwrap_or_else(|_| "tb_lite".to_string())
            .to_ascii_lowercase();
        match raw.as_str() {
            "" | "tb" | "tb_lite" | "tree_borrows" => ActiveModel::TbLite,
            "none" | "off" => ActiveModel::None,
            // Keep unknown values non-fatal; default to the current model.
            _ => ActiveModel::TbLite,
        }
    })
}

pub(crate) fn active_alias_model() -> &'static dyn AliasModel {
    match active_model_choice() {
        ActiveModel::TbLite => &TB_LITE_MODEL,
        ActiveModel::None => &NO_ALIAS_MODEL,
    }
}

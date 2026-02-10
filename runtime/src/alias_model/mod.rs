use std::collections::HashMap;
use std::sync::OnceLock;

use crate::{PtrKind, TagMeta};

mod stacked_borrows_lite;
mod tree_borrows_lite;

pub(crate) use stacked_borrows_lite::StackedBorrowsLiteModel;
pub(crate) use tree_borrows_lite::TreeBorrowsLiteModel;

#[derive(Copy, Clone, Debug)]
pub(crate) enum AliasAccessKind {
    Read,
    Write,
}

pub(crate) trait AliasModel: Sync {
    fn name(&self) -> &'static str;

    fn violation_kind(&self) -> &'static str {
        "STACKED_BORROWS_VIOLATION"
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

    /// Called when a callee consumes a caller-pushed argument tag.
    /// Alias models can use this to seed call-scope metadata (e.g., protectors).
    fn on_call_arg_taken(&self, _callee_id: u64, _parent_tag: u64) {}

    /// Called at instrumented function return.
    fn on_call_exit(&self, _callee_id: u64) {}

    fn find_ref_ancestor_tag(&self, _tmap: &HashMap<u64, TagMeta>, _tag: u64) -> Option<u64> {
        None
    }

    fn check_access(
        &self,
        _sb_tag: u64,
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
static SB_LITE_MODEL: StackedBorrowsLiteModel = StackedBorrowsLiteModel;
static TB_LITE_MODEL: TreeBorrowsLiteModel = TreeBorrowsLiteModel;

#[derive(Copy, Clone, Debug)]
enum ActiveModel {
    SbLite,
    TbLite,
    None,
}

fn active_model_choice() -> ActiveModel {
    static ACTIVE: OnceLock<ActiveModel> = OnceLock::new();
    *ACTIVE.get_or_init(|| {
        let raw = std::env::var("RZ_ALIAS_MODEL")
            .unwrap_or_else(|_| "sb_lite".to_string())
            .to_ascii_lowercase();
        match raw.as_str() {
            "" | "sb" | "sb_lite" | "stacked_borrows" => ActiveModel::SbLite,
            "tb" | "tb_lite" | "tree_borrows" => ActiveModel::TbLite,
            "none" | "off" => ActiveModel::None,
            // Keep unknown values non-fatal; default to the current model.
            _ => ActiveModel::SbLite,
        }
    })
}

pub(crate) fn active_alias_model() -> &'static dyn AliasModel {
    match active_model_choice() {
        ActiveModel::SbLite => &SB_LITE_MODEL,
        ActiveModel::TbLite => &TB_LITE_MODEL,
        ActiveModel::None => &NO_ALIAS_MODEL,
    }
}

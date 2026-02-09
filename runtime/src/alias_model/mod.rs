use std::collections::HashMap;
use std::sync::OnceLock;

use crate::{PtrKind, TagMeta};

mod stacked_borrows_lite;

pub(crate) use stacked_borrows_lite::StackedBorrowsLiteModel;

#[derive(Copy, Clone, Debug)]
pub(crate) enum AliasAccessKind {
    Read,
    Write,
}

pub(crate) trait AliasModel: Sync {
    fn name(&self) -> &'static str;

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

#[derive(Copy, Clone, Debug)]
enum ActiveModel {
    SbLite,
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
            "none" | "off" => ActiveModel::None,
            // Keep unknown values non-fatal; default to the current model.
            _ => ActiveModel::SbLite,
        }
    })
}

pub(crate) fn active_alias_model() -> &'static dyn AliasModel {
    match active_model_choice() {
        ActiveModel::SbLite => &SB_LITE_MODEL,
        ActiveModel::None => &NO_ALIAS_MODEL,
    }
}

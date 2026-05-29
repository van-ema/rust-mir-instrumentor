use super::*;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(in crate::instrumentation) enum AllocShimKind {
    No,
    Alloc,
    AllocZeroed,
    Dealloc,
    Realloc,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(in crate::instrumentation) enum CallEffect {
    /// No memory / pointer-tracking relevant effect (e.g., ptr::is_null).
    Ignore,
    /// memcpy/memmove-style (read src, write dst).
    MemCopy,
    /// memset-style (write dst).
    MemSet,
    /// Ptr load/store wrappers/intrinsics (plain or volatile).
    Load,
    Store,
    LoadUnaligned,
    StoreUnaligned,
    /// Pointer derivation wrappers that return a pointer derived from arg0 (fresh tag, parent linkage).
    PtrDerive,
    /// Pointer-returning helpers that create a raw root from an integer/exposed address.
    ExposedProvenanceRoot,
    /// Wrapper constructors that return a non-pointer carrier whose pointer leaves come from arg0.
    CarrierCopyArg0,
    /// Iterator-style helpers that return aggregate reference items derived from arg0's pointee carrier.
    RefRetFromArg0PointeeLeafs,
    /// Box boundary modeling.
    BoxIntoRaw,
    BoxFromRaw,
    /// Allocator shims/wrappers.
    AllocShim(AllocShimKind),
    /// Not recognized.
    Unknown,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum MatchKind {
    Contains,
    EndsWith,
}

#[derive(Copy, Clone, Debug)]
struct EffectRule {
    kind1: MatchKind,
    needle1: &'static str,
    // Optional second condition: both must match.
    kind2: Option<MatchKind>,
    needle2: Option<&'static str>,
    effect: CallEffect,
}

impl EffectRule {
    const fn one(kind: MatchKind, needle: &'static str, effect: CallEffect) -> Self {
        Self {
            kind1: kind,
            needle1: needle,
            kind2: None,
            needle2: None,
            effect,
        }
    }

    const fn two(
        kind1: MatchKind,
        needle1: &'static str,
        kind2: MatchKind,
        needle2: &'static str,
        effect: CallEffect,
    ) -> Self {
        Self {
            kind1,
            needle1,
            kind2: Some(kind2),
            needle2: Some(needle2),
            effect,
        }
    }
}

fn normalize_def_path(def_path: &str) -> String {
    let mut base = def_path.to_string();
    if let Some(stripped) = strip_trait_impl_prefix(def_path) {
        base = stripped;
    }

    let mut out = String::with_capacity(base.len());
    let mut depth = 0usize;
    let mut colon_run = 0usize;
    for ch in base.chars() {
        match ch {
            '<' => {
                depth += 1;
            }
            '>' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            _ if depth > 0 => {
                // Skip generic args.
            }
            c => {
                if c.is_whitespace() {
                    continue;
                }
                if c == ':' {
                    colon_run += 1;
                    if colon_run > 2 {
                        continue;
                    }
                } else {
                    colon_run = 0;
                }
                out.push(c);
            }
        }
    }

    out
}

fn strip_trait_impl_prefix(def_path: &str) -> Option<String> {
    if !def_path.starts_with('<') {
        return None;
    }

    let mut depth = 0usize;
    let mut end_idx = None;
    for (idx, ch) in def_path.char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => {
                if depth > 0 {
                    depth -= 1;
                    if depth == 0 {
                        end_idx = Some(idx);
                        break;
                    }
                }
            }
            _ => {}
        }
    }

    let end_idx = end_idx?;
    let inner = &def_path[1..end_idx];
    let rest = &def_path[(end_idx + 1)..];
    let as_pos = inner.find(" as ")?;
    let trait_part = inner[(as_pos + 4)..].trim();
    if trait_part.is_empty() {
        return None;
    }

    Some(format!("{}{}", trait_part, rest))
}

// Order matters: first match wins.
// These rules cover "simple" std/core wrapper classification that is purely path-string based.
static CALL_EFFECT_RULES: &[EffectRule] = &[
    // ---- Allocator shims & wrappers (order matters) ----

    // Low-level shims.
    EffectRule::one(
        MatchKind::Contains,
        "__rust_alloc_zeroed",
        CallEffect::AllocShim(AllocShimKind::AllocZeroed),
    ),
    EffectRule::one(
        MatchKind::Contains,
        "__rust_alloc",
        CallEffect::AllocShim(AllocShimKind::Alloc),
    ),
    EffectRule::one(
        MatchKind::Contains,
        "__rust_dealloc",
        CallEffect::AllocShim(AllocShimKind::Dealloc),
    ),
    EffectRule::one(
        MatchKind::Contains,
        "__rust_realloc",
        CallEffect::AllocShim(AllocShimKind::Realloc),
    ),
    // alloc::alloc wrappers.
    EffectRule::one(
        MatchKind::Contains,
        "alloc::alloc::exchange_malloc",
        CallEffect::AllocShim(AllocShimKind::Alloc),
    ),
    EffectRule::one(
        MatchKind::Contains,
        "alloc::alloc::alloc_zeroed",
        CallEffect::AllocShim(AllocShimKind::AllocZeroed),
    ),
    // Keep this after alloc_zeroed so it doesn't catch it first.
    EffectRule::one(
        MatchKind::Contains,
        "alloc::alloc::alloc",
        CallEffect::AllocShim(AllocShimKind::Alloc),
    ),
    EffectRule::one(
        MatchKind::Contains,
        "alloc::alloc::dealloc",
        CallEffect::AllocShim(AllocShimKind::Dealloc),
    ),
    EffectRule::one(
        MatchKind::Contains,
        "alloc::alloc::realloc",
        CallEffect::AllocShim(AllocShimKind::Realloc),
    ),
    // std::alloc wrappers (often take `Layout`).
    EffectRule::one(
        MatchKind::Contains,
        "std::alloc::alloc_zeroed",
        CallEffect::AllocShim(AllocShimKind::AllocZeroed),
    ),
    // Keep this after alloc_zeroed so it doesn't catch it first.
    EffectRule::one(
        MatchKind::Contains,
        "std::alloc::alloc",
        CallEffect::AllocShim(AllocShimKind::Alloc),
    ),
    EffectRule::one(
        MatchKind::Contains,
        "std::alloc::dealloc",
        CallEffect::AllocShim(AllocShimKind::Dealloc),
    ),
    EffectRule::one(
        MatchKind::Contains,
        "std::alloc::realloc",
        CallEffect::AllocShim(AllocShimKind::Realloc),
    ),
    // No-op helpers.
    EffectRule::one(MatchKind::EndsWith, "::is_null", CallEffect::Ignore),
    EffectRule::one(MatchKind::Contains, "::ptr::eq", CallEffect::Ignore),
    EffectRule::one(MatchKind::Contains, "::ptr::addr_eq", CallEffect::Ignore),
    EffectRule::one(MatchKind::Contains, "::ptr::null", CallEffect::Ignore),
    EffectRule::one(MatchKind::Contains, "::ptr::null_mut", CallEffect::Ignore),
    EffectRule::one(
        MatchKind::Contains,
        "::without_provenance",
        CallEffect::ExposedProvenanceRoot,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::with_exposed_provenance",
        CallEffect::ExposedProvenanceRoot,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::const_ptr",
        MatchKind::EndsWith,
        "::eq",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::mut_ptr",
        MatchKind::EndsWith,
        "::eq",
        CallEffect::Ignore,
    ),
    // ---- Common std/core helpers (suppress unknown-call noise) ----

    // Deref/DerefMut return a reference derived from self.
    EffectRule::two(
        MatchKind::Contains,
        "::ops::deref::Deref",
        MatchKind::EndsWith,
        "::deref",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ops::deref::DerefMut",
        MatchKind::EndsWith,
        "::deref_mut",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ops::Deref",
        MatchKind::EndsWith,
        "::deref",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ops::DerefMut",
        MatchKind::EndsWith,
        "::deref_mut",
        CallEffect::PtrDerive,
    ),
    // Iterator metadata helpers.
    EffectRule::two(
        MatchKind::Contains,
        "::iter::traits::iterator::Iterator",
        MatchKind::EndsWith,
        "::size_hint",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::iter::Iterator",
        MatchKind::EndsWith,
        "::size_hint",
        CallEffect::Ignore,
    ),
    // Opaque iterator adapters that return another carrier derived from self.
    EffectRule::two(
        MatchKind::Contains,
        "::iter::traits::iterator::Iterator",
        MatchKind::EndsWith,
        "::take",
        CallEffect::CarrierCopyArg0,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::iter::Iterator",
        MatchKind::EndsWith,
        "::take",
        CallEffect::CarrierCopyArg0,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::iter::traits::collect::IntoIterator",
        MatchKind::EndsWith,
        "::into_iter",
        CallEffect::CarrierCopyArg0,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::iter::IntoIterator",
        MatchKind::EndsWith,
        "::into_iter",
        CallEffect::CarrierCopyArg0,
    ),
    // `next` is only modeled structurally when the returned aggregate carries reference leaves
    // and those leaves can be recovered from the pointee carrier behind `&mut self`.
    EffectRule::two(
        MatchKind::Contains,
        "::iter::traits::iterator::Iterator",
        MatchKind::EndsWith,
        "::next",
        CallEffect::RefRetFromArg0PointeeLeafs,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::iter::Iterator",
        MatchKind::EndsWith,
        "::next",
        CallEffect::RefRetFromArg0PointeeLeafs,
    ),
    // Slice helpers that return view/iterator carriers derived from arg0.
    EffectRule::two(
        MatchKind::Contains,
        "::slice::<impl [",
        MatchKind::EndsWith,
        "::iter",
        CallEffect::CarrierCopyArg0,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::slice::<impl [",
        MatchKind::EndsWith,
        "::iter_mut",
        CallEffect::CarrierCopyArg0,
    ),
    // ---- PtrDerive wrappers (pointer arithmetic + slice/vec pointer extraction) ----

    // Pointer arithmetic wrappers: constrain to `::ptr::` and method name.
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::add",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::sub",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::offset",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::wrapping_add",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::wrapping_sub",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::wrapping_offset",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::byte_add",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::byte_sub",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::wrapping_byte_add",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::wrapping_byte_sub",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::cast",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::cast_mut",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::cast_const",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::offset_from",
        CallEffect::Ignore,
    ),
    // Slice pointer extraction wrappers.
    EffectRule::two(
        MatchKind::Contains,
        "::slice::<impl [",
        MatchKind::EndsWith,
        "::as_ptr",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::slice::<impl [",
        MatchKind::EndsWith,
        "::as_mut_ptr",
        CallEffect::PtrDerive,
    ),
    // Raw pointer creation helpers.
    EffectRule::one(
        MatchKind::Contains,
        "::ptr::from_ref",
        CallEffect::PtrDerive,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::ptr::from_mut",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::cell::UnsafeCell",
        MatchKind::EndsWith,
        "::get",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::cell::SyncUnsafeCell",
        MatchKind::EndsWith,
        "::get",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::NonNull",
        MatchKind::EndsWith,
        "::new",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::NonNull",
        MatchKind::EndsWith,
        "::new_unchecked",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::NonNull",
        MatchKind::EndsWith,
        "::as_ptr",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::NonNull",
        MatchKind::EndsWith,
        "::as_mut",
        CallEffect::PtrDerive,
    ),
    // Vec pointer extraction wrappers. Covers `alloc::vec::Vec` and `std::vec::Vec`, including monomorphized forms.
    EffectRule::two(
        MatchKind::Contains,
        "::vec::Vec",
        MatchKind::EndsWith,
        "::as_ptr",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::vec::Vec",
        MatchKind::EndsWith,
        "::as_mut_ptr",
        CallEffect::PtrDerive,
    ),
    // Transmute-style helpers returning pointers should preserve lineage.
    EffectRule::one(
        MatchKind::Contains,
        "::mem::transmute",
        CallEffect::PtrDerive,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::transmute",
        CallEffect::PtrDerive,
    ),
    // Volatile wrappers (free functions).
    EffectRule::one(
        MatchKind::Contains,
        "::ptr::read_volatile",
        CallEffect::Load,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::ptr::write_volatile",
        CallEffect::Store,
    ),
    // Volatile intrinsics.
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::volatile_load",
        CallEffect::Load,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::volatile_store",
        CallEffect::Store,
    ),
    // Core intrinsics used by mem::replace and similar wrappers.
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::read_via_copy",
        CallEffect::Load,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::write_via_move",
        CallEffect::Store,
    ),
    // Memset-like.
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::write_bytes",
        CallEffect::MemSet,
    ),
    // Method-style wrappers (e.g. std::ptr::mut_ptr::<impl *mut T>::write_bytes)
    EffectRule::one(MatchKind::EndsWith, "::write_bytes", CallEffect::MemSet),
    // Plain wrappers.
    // Use suffix matching for `read`/`write` so we don't accidentally match `write_bytes`/`read_bytes`.
    EffectRule::one(
        MatchKind::Contains,
        "::ptr::read_unaligned",
        CallEffect::LoadUnaligned,
    ),
    EffectRule::one(MatchKind::EndsWith, "::read", CallEffect::Load),
    EffectRule::one(
        MatchKind::Contains,
        "::ptr::write_unaligned",
        CallEffect::StoreUnaligned,
    ),
    EffectRule::one(MatchKind::EndsWith, "::drop_in_place", CallEffect::Store),
    EffectRule::two(
        MatchKind::Contains,
        "::cell::Cell",
        MatchKind::EndsWith,
        "::set",
        CallEffect::Store,
    ),
    EffectRule::one(MatchKind::EndsWith, "::write", CallEffect::Store),
    // Memcpy/memmove-like.
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::copy_nonoverlapping",
        CallEffect::MemCopy,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::copy",
        CallEffect::MemCopy,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::ptr::copy_nonoverlapping",
        CallEffect::MemCopy,
    ),
    EffectRule::one(MatchKind::Contains, "::ptr::copy", CallEffect::MemCopy),
    // Method-style wrappers (e.g. std::ptr::mut_ptr::<impl *mut T>::copy_nonoverlapping)
    EffectRule::one(
        MatchKind::EndsWith,
        "::copy_nonoverlapping",
        CallEffect::MemCopy,
    ),
    // Method-style wrappers (e.g. std::ptr::mut_ptr::<impl *mut T>::copy)
    EffectRule::one(MatchKind::EndsWith, "::copy", CallEffect::MemCopy),
    // ---- Common pure helpers / structural carriers ----
    // RangeBounds::{start,end}_bound return Bound<&usize>-style carriers that must stay in the
    // range object's local family instead of floating as untracked helper results.
    EffectRule::two(
        MatchKind::Contains,
        "::ops::RangeBounds",
        MatchKind::EndsWith,
        "::start_bound",
        CallEffect::CarrierCopyArg0,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ops::RangeBounds",
        MatchKind::EndsWith,
        "::end_bound",
        CallEffect::CarrierCopyArg0,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ops::Range",
        MatchKind::EndsWith,
        "::contains",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::option::Option",
        MatchKind::EndsWith,
        "::expect",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::result::Result",
        MatchKind::EndsWith,
        "::is_ok",
        CallEffect::Ignore,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::mem::size_of_val",
        CallEffect::Ignore,
    ),
    EffectRule::one(MatchKind::Contains, "::mem::take", CallEffect::Ignore),
    EffectRule::one(MatchKind::Contains, "::mem::replace", CallEffect::Ignore),
    EffectRule::one(
        MatchKind::Contains,
        "::panicking::assert_failed",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::cmp::PartialEq",
        MatchKind::EndsWith,
        "::eq",
        CallEffect::Load,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::cmp::PartialOrd",
        MatchKind::EndsWith,
        "::partial_cmp",
        CallEffect::Load,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::cmp::Ord",
        MatchKind::EndsWith,
        "::cmp",
        CallEffect::Load,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::hash::Hash",
        MatchKind::EndsWith,
        "::hash",
        CallEffect::Load,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::hash::impls",
        MatchKind::EndsWith,
        "::hash",
        CallEffect::Load,
    ),
    // AsRef/AsMut return borrowed pointers.
    EffectRule::two(
        MatchKind::Contains,
        "::convert::AsRef",
        MatchKind::EndsWith,
        "::as_ref",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::convert::AsMut",
        MatchKind::EndsWith,
        "::as_mut",
        CallEffect::PtrDerive,
    ),
    // Index/IndexMut return references into the receiver.
    EffectRule::two(
        MatchKind::Contains,
        "::ops::IndexMut",
        MatchKind::EndsWith,
        "::index_mut",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ops::Index",
        MatchKind::EndsWith,
        "::index",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "SliceIndex",
        MatchKind::EndsWith,
        "::index_mut",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "SliceIndex",
        MatchKind::EndsWith,
        "::index",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::slice::index::<impl",
        MatchKind::EndsWith,
        "::index_mut",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::slice::index::<impl",
        MatchKind::EndsWith,
        "::index",
        CallEffect::PtrDerive,
    ),
    // Slice helpers.
    EffectRule::two(
        MatchKind::Contains,
        "::slice::<impl [",
        MatchKind::EndsWith,
        "::get",
        CallEffect::CarrierCopyArg0,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::slice::<impl [",
        MatchKind::EndsWith,
        "::get_mut",
        CallEffect::CarrierCopyArg0,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::slice::<impl [",
        MatchKind::EndsWith,
        "::last_mut",
        CallEffect::CarrierCopyArg0,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::slice::<impl [",
        MatchKind::EndsWith,
        "::len",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::slice::<impl [",
        MatchKind::EndsWith,
        "::is_empty",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::slice::<impl [",
        MatchKind::EndsWith,
        "::split_at",
        CallEffect::CarrierCopyArg0,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::slice::<impl [",
        MatchKind::EndsWith,
        "::split_at_mut",
        CallEffect::CarrierCopyArg0,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::slice::<impl [",
        MatchKind::EndsWith,
        "::copy_from_slice",
        CallEffect::Ignore,
    ),
    // from_raw_parts{,_mut} return slice references derived from the base pointer.
    // This is heavily used by unsafe code; modeling it as PtrDerive avoids losing lineage.
    EffectRule::one(
        MatchKind::Contains,
        "::slice::from_raw_parts",
        CallEffect::PtrDerive,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::slice::from_raw_parts_mut",
        CallEffect::PtrDerive,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::slice::raw::from_raw_parts",
        CallEffect::PtrDerive,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::slice::raw::from_raw_parts_mut",
        CallEffect::PtrDerive,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::ptr::slice_from_raw_parts",
        CallEffect::PtrDerive,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::ptr::slice_from_raw_parts_mut",
        CallEffect::PtrDerive,
    ),
    // Vec helpers (metadata + length management).
    EffectRule::two(
        MatchKind::Contains,
        "::vec::Vec",
        MatchKind::EndsWith,
        "::len",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::vec::Vec",
        MatchKind::EndsWith,
        "::capacity",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::vec::Vec",
        MatchKind::EndsWith,
        "::is_empty",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::vec::Vec",
        MatchKind::EndsWith,
        "::set_len",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::vec::Vec",
        MatchKind::EndsWith,
        "::from_raw_parts",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::vec::Vec",
        MatchKind::EndsWith,
        "::from_raw_parts_in",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "alloc::slice::<impl [",
        MatchKind::EndsWith,
        "::to_vec",
        CallEffect::Ignore,
    ),
    // VecDeque helpers.
    EffectRule::two(
        MatchKind::Contains,
        "::collections::VecDeque",
        MatchKind::EndsWith,
        "::len",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::collections::VecDeque",
        MatchKind::EndsWith,
        "::is_empty",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::collections::VecDeque",
        MatchKind::EndsWith,
        "::as_slices",
        CallEffect::CarrierCopyArg0,
    ),
    // String / str helpers.
    EffectRule::two(
        MatchKind::Contains,
        "::str::<impl str>",
        MatchKind::EndsWith,
        "::len",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::str::<impl str>",
        MatchKind::EndsWith,
        "::as_bytes",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::string::String",
        MatchKind::EndsWith,
        "::as_bytes",
        CallEffect::Ignore,
    ),
    // IO helpers.
    EffectRule::two(
        MatchKind::Contains,
        "::io::Cursor",
        MatchKind::EndsWith,
        "::get_ref",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::io::Cursor",
        MatchKind::EndsWith,
        "::position",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::io::Cursor",
        MatchKind::EndsWith,
        "::set_position",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::io::IoSlice",
        MatchKind::EndsWith,
        "::new",
        CallEffect::Ignore,
    ),
    // Pure state queries that only read scalar state from a receiver. These are common in parser
    // and codec hot paths and do not justify materializing a tracked temporary shared borrow.

    // Atomic ops (load/store vs RMW).
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::Atomic",
        MatchKind::EndsWith,
        "::load",
        CallEffect::Load,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::Atomic",
        MatchKind::EndsWith,
        "::store",
        CallEffect::Store,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::Atomic",
        MatchKind::EndsWith,
        "::swap",
        CallEffect::Store,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::Atomic",
        MatchKind::EndsWith,
        "::compare_exchange",
        CallEffect::Store,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::Atomic",
        MatchKind::EndsWith,
        "::compare_exchange_weak",
        CallEffect::Store,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::Atomic",
        MatchKind::EndsWith,
        "::fetch_add",
        CallEffect::Store,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::Atomic",
        MatchKind::EndsWith,
        "::fetch_sub",
        CallEffect::Store,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::Atomic",
        MatchKind::EndsWith,
        "::fetch_and",
        CallEffect::Store,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::Atomic",
        MatchKind::EndsWith,
        "::fetch_or",
        CallEffect::Store,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::Atomic",
        MatchKind::EndsWith,
        "::fetch_xor",
        CallEffect::Store,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::Atomic",
        MatchKind::EndsWith,
        "::fetch_nand",
        CallEffect::Store,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::Atomic",
        MatchKind::EndsWith,
        "::fetch_max",
        CallEffect::Store,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::Atomic",
        MatchKind::EndsWith,
        "::fetch_min",
        CallEffect::Store,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::AtomicPtr",
        MatchKind::EndsWith,
        "::new",
        CallEffect::CarrierCopyArg0,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::AtomicPtr",
        MatchKind::EndsWith,
        "::get_mut",
        CallEffect::Ignore,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::sync::atomic::atomic_load",
        CallEffect::Load,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::sync::atomic::atomic_store",
        CallEffect::Store,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::sync::atomic::atomic_compare_exchange",
        CallEffect::Store,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::sync::atomic::atomic_xadd",
        CallEffect::Store,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::sync::atomic::atomic_xsub",
        CallEffect::Store,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::atomic_load",
        CallEffect::Load,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::atomic_store",
        CallEffect::Store,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::atomic_",
        CallEffect::Store,
    ),
    // Intrinsics + pointer helpers seen in optimized builds.
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::arith_offset",
        CallEffect::PtrDerive,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::ptr_offset_from",
        CallEffect::Ignore,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::ptr_offset_from_unsigned",
        CallEffect::Ignore,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::compare_bytes",
        CallEffect::Ignore,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::size_of_val",
        CallEffect::Ignore,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::align_of_val",
        CallEffect::Ignore,
    ),
];

impl MyOptimizationPass {
    pub(in crate::instrumentation) fn normalize_def_path(&self, def_path: &str) -> String {
        normalize_def_path(def_path)
    }

    pub(in crate::instrumentation) fn match_call_effect_rule(
        &self,
        def_path: &str,
    ) -> Option<CallEffect> {
        let def_path_norm = self.normalize_def_path(def_path);
        // Strip only a *trailing* monomorphization like `::<T>`.
        // Do NOT strip generic args that appear in the middle of a path like
        // `std::vec::Vec::<T, A>::as_mut_ptr`, otherwise we lose the method suffix.
        let def_path_no_trailing_mono = {
            let s = def_path;
            if !s.ends_with('>') {
                s
            } else if let Some(pos) = s.rfind("::<") {
                // Only treat it as a trailing monomorphization if there is no further module separator
                // after the `::<` (excluding the `::` in the `::<` itself).
                let after = &s[(pos + 3)..];
                if after.contains("::") {
                    s
                } else {
                    &s[..pos]
                }
            } else {
                s
            }
        };

        for r in CALL_EFFECT_RULES {
            let m1 = match r.kind1 {
                MatchKind::Contains => {
                    def_path.contains(r.needle1) || def_path_norm.contains(r.needle1)
                }
                MatchKind::EndsWith => {
                    def_path.ends_with(r.needle1)
                        || def_path_no_trailing_mono.ends_with(r.needle1)
                        || def_path_norm.ends_with(r.needle1)
                }
            };
            if !m1 {
                continue;
            }

            if let (Some(k2), Some(n2)) = (r.kind2, r.needle2) {
                let m2 = match k2 {
                    MatchKind::Contains => def_path.contains(n2) || def_path_norm.contains(n2),
                    MatchKind::EndsWith => {
                        def_path.ends_with(n2)
                            || def_path_no_trailing_mono.ends_with(n2)
                            || def_path_norm.ends_with(n2)
                    }
                };
                if !m2 {
                    continue;
                }
            }

            return Some(r.effect);
        }
        None
    }
    /// Centralized call-effect classifier ("table").
    ///
    /// This MUST be kept consistent with instrumentation emission so that
    /// `warn_unknown_call_if_needed` does not drift from actual handling.
    pub(in crate::instrumentation) fn classify_call_effect(&self, def_path: &str) -> CallEffect {
        if def_path.contains("::black_box") {
            return CallEffect::PtrDerive;
        }
        if self.is_box_into_raw_wrapper(def_path) {
            return CallEffect::BoxIntoRaw;
        }
        if self.is_box_from_raw_wrapper(def_path) {
            return CallEffect::BoxFromRaw;
        }

        if let Some(eff) = self.match_call_effect_rule(def_path) {
            return eff;
        }

        CallEffect::Unknown
    }

    pub(in crate::instrumentation) fn classify_instrumented_call_effect_from_summary<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        args: &Box<[Spanned<Operand<'tcx>>]>,
        destination: &Place<'tcx>,
        summary: &unsafe_dataflow::UnsafeFunctionSummary,
    ) -> Option<CallEffect> {
        let dst_ty = destination.ty(&body.local_decls, tcx).ty;
        if !self.is_pointer_ty(dst_ty) {
            return None;
        }

        if summary
            .ptr_args()
            .iter()
            .any(|entry| entry.reaches_direct_sink() || entry.escapes_to_unknown_boundary())
        {
            return None;
        }

        let mut forwarded = summary
            .ptr_args()
            .iter()
            .filter(|entry| entry.forwarded_to_return());
        let forwarded_arg = forwarded.next()?;
        if forwarded.next().is_some() {
            return None;
        }

        let Some(arg) = args.get(forwarded_arg.arg_index()) else {
            return None;
        };
        let Some(arg_place) = self.place_from_operand(&arg.node) else {
            return None;
        };
        let arg_ty = arg_place.ty(&body.local_decls, tcx).ty;
        if !self.is_pointer_ty(arg_ty) {
            return None;
        }
        if let TyKind::Ref(_, pointee_ty, _) = arg_ty.kind() {
            if !self.is_pointer_ty(*pointee_ty) {
                return None;
            }
        }

        Some(CallEffect::PtrDerive)
    }
}

fn call_effect_label(effect: CallEffect) -> &'static str {
    match effect {
        CallEffect::Ignore => "Ignore",
        CallEffect::MemCopy => "MemCopy",
        CallEffect::MemSet => "MemSet",
        CallEffect::Load => "Load",
        CallEffect::Store => "Store",
        CallEffect::LoadUnaligned => "LoadUnaligned",
        CallEffect::StoreUnaligned => "StoreUnaligned",
        CallEffect::PtrDerive => "PtrDerive",
        CallEffect::ExposedProvenanceRoot => "ExposedProvenanceRoot",
        CallEffect::CarrierCopyArg0 => "CarrierCopyArg0",
        CallEffect::RefRetFromArg0PointeeLeafs => "RefRetFromArg0PointeeLeafs",
        CallEffect::BoxIntoRaw => "BoxIntoRaw",
        CallEffect::BoxFromRaw => "BoxFromRaw",
        CallEffect::AllocShim(AllocShimKind::Alloc) => "AllocShim(Alloc)",
        CallEffect::AllocShim(AllocShimKind::AllocZeroed) => "AllocShim(AllocZeroed)",
        CallEffect::AllocShim(AllocShimKind::Dealloc) => "AllocShim(Dealloc)",
        CallEffect::AllocShim(AllocShimKind::Realloc) => "AllocShim(Realloc)",
        CallEffect::AllocShim(AllocShimKind::No) => "AllocShim(No)",
        CallEffect::Unknown => "Unknown",
    }
}

pub(crate) fn debug_classify_call_effect(def_path: &str) -> &'static str {
    let pass = MyOptimizationPass;
    let effect = pass
        .match_call_effect_rule(def_path)
        .unwrap_or(CallEffect::Unknown);
    call_effect_label(effect)
}

#[cfg(test)]
mod tests {
    use super::{CallEffect, MyOptimizationPass};

    fn effect_for(def_path: &str) -> CallEffect {
        MyOptimizationPass
            .match_call_effect_rule(def_path)
            .unwrap_or(CallEffect::Unknown)
    }

    #[test]
    fn classify_common_helpers() {
        assert_eq!(
            effect_for("core::slice::<impl [T]>::get"),
            CallEffect::CarrierCopyArg0
        );
        assert_eq!(
            effect_for("core::slice::<impl [T]>::iter"),
            CallEffect::CarrierCopyArg0
        );
        assert_eq!(
            effect_for("core::slice::<impl [T]>::iter_mut"),
            CallEffect::CarrierCopyArg0
        );
        assert_eq!(
            effect_for("core::iter::Iterator::take"),
            CallEffect::CarrierCopyArg0
        );
        assert_eq!(
            effect_for("core::iter::IntoIterator::into_iter"),
            CallEffect::CarrierCopyArg0
        );
        assert_eq!(
            effect_for("core::iter::Iterator::next"),
            CallEffect::RefRetFromArg0PointeeLeafs
        );
        assert_eq!(
            effect_for("core::slice::<impl [T]>::is_empty"),
            CallEffect::Ignore
        );
        assert_eq!(
            effect_for("core::slice::<impl [T]>::len"),
            CallEffect::Ignore
        );
        assert_eq!(
            effect_for("core::slice::<impl [T]>::split_at"),
            CallEffect::CarrierCopyArg0
        );
        assert_eq!(
            effect_for("core::slice::<impl [T]>::split_at_mut"),
            CallEffect::CarrierCopyArg0
        );
        assert_eq!(
            effect_for("core::str::<impl str>::as_bytes"),
            CallEffect::PtrDerive
        );
        assert_eq!(
            effect_for("core::slice::index::<impl core::ops::Index<I> for [T]>::index"),
            CallEffect::PtrDerive
        );
        assert_eq!(
            effect_for("core::slice::index::<impl core::ops::IndexMut<I> for [T]>::index_mut"),
            CallEffect::PtrDerive
        );
        assert_eq!(
            effect_for(
                "core::slice::index::<impl std::slice::SliceIndex<[u8]> for std::ops::Range<usize>>::index_mut"
            ),
            CallEffect::PtrDerive
        );
        assert_eq!(
            effect_for("<std::ops::Range<usize> as std::slice::SliceIndex<[u8]>>::index_mut"),
            CallEffect::PtrDerive
        );
        assert_eq!(
            effect_for("core::slice::index::<impl std::slice::SliceIndex<[u8]> for usize>::index"),
            CallEffect::PtrDerive
        );
        assert_eq!(
            effect_for("<usize as std::slice::SliceIndex<[u8]>>::index"),
            CallEffect::PtrDerive
        );
        assert_eq!(
            effect_for("core::ops::RangeBounds::start_bound"),
            CallEffect::CarrierCopyArg0
        );
        assert_eq!(
            effect_for("core::ops::RangeBounds::end_bound"),
            CallEffect::CarrierCopyArg0
        );
        assert_eq!(
            effect_for("alloc::vec::Vec::<T, A>::len"),
            CallEffect::Ignore
        );
        assert_eq!(
            effect_for("core::convert::AsRef::as_ref"),
            CallEffect::PtrDerive
        );
        assert_eq!(
            effect_for("<T as core::convert::AsRef<U>>::as_ref"),
            CallEffect::PtrDerive
        );
        assert_eq!(
            effect_for("core::ptr::NonNull::<T>::as_ptr"),
            CallEffect::PtrDerive
        );
        assert_eq!(
            effect_for("core::sync::atomic::AtomicPtr::<T>::new"),
            CallEffect::CarrierCopyArg0
        );
        assert_eq!(effect_for("core::ptr::null"), CallEffect::Ignore);
        assert_eq!(
            effect_for("std::io::Cursor::<T>::get_ref"),
            CallEffect::PtrDerive
        );
        assert_eq!(
            effect_for("std::io::Cursor::<T>::position"),
            CallEffect::Ignore
        );
        assert_eq!(
            effect_for("alloc::collections::VecDeque::<T>::as_slices"),
            CallEffect::CarrierCopyArg0
        );
        assert_eq!(
            effect_for("core::sync::atomic::AtomicUsize::load"),
            CallEffect::Load
        );
        assert_eq!(
            effect_for("core::sync::atomic::AtomicUsize::compare_exchange"),
            CallEffect::Store
        );
        assert_eq!(
            effect_for("core::intrinsics::arith_offset"),
            CallEffect::PtrDerive
        );
        assert_eq!(
            effect_for("core::intrinsics::read_via_copy"),
            CallEffect::Load
        );
        assert_eq!(
            effect_for("core::intrinsics::write_via_move"),
            CallEffect::Store
        );
        assert_eq!(
            MyOptimizationPass.classify_call_effect("std::hint::black_box"),
            CallEffect::PtrDerive
        );
        assert_eq!(
            effect_for("<alloc::vec::Vec<T, A> as core::ops::Index<I>>::index"),
            CallEffect::PtrDerive
        );
    }
}

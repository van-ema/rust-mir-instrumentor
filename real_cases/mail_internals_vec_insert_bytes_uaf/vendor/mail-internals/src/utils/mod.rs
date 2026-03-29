//! Some more general utilities.
//!
//! Or with other words, thinks which
//! (currently) have no other place to
//! be placed in.
use std::any::TypeId;
use std::cell::RefCell;
use std::mem;
use std::fmt::{self, Debug};


/// Helper for implementing debug for an iterable think where the think on itself is irrelevant.
pub struct DebugIterableOpaque<I> {
    one_use_inner: RefCell<I>
}

impl<I> DebugIterableOpaque<I> {
    pub fn new(one_use_inner: I) -> Self {
        let one_use_inner = RefCell::new(one_use_inner);
        DebugIterableOpaque { one_use_inner }
    }
}
impl<I> Debug for DebugIterableOpaque<I>
    where I: Iterator, I::Item: Debug
{
    fn fmt(&self, fter: &mut fmt::Formatter) -> fmt::Result {
        let mut borrow = self.one_use_inner.borrow_mut();
        fter.debug_list().entries(&mut *borrow).finish()
    }
}


//FIXME[rust/fat pointer cast]: make it ?Sized once it's supported by rust
///
/// Used to undo type erasure in a generic context,
/// roughly semantically eqivalent to creating a `&Any`
/// type object from the input and then using `downcast_ref::<EXP>()`,
/// except that it does not require the cration of a
/// trait object as a step inbetween.
///
/// Note:
/// This function can be used for some form of specialisation,
/// (not just in a performence sense) but all "specialization path"
/// have to be known when writing the unspeciallized version and
/// it is easy to make functions behave in a unexpected (but safe)
/// way so use with care.
///
///
#[inline(always)]
pub fn uneraser_ref<GOT: 'static, EXP: 'static>(inp: &GOT ) -> Option<&EXP>  {
    if TypeId::of::<GOT>() == TypeId::of::<EXP>() {
        //SAFE: the GOT type is exact the same as the EXP type,
        // the compiler just does not know this due to type erasure wrt.
        // generic types
        let res: &EXP = unsafe { mem::transmute::<&GOT, &EXP>(inp) };
        Some( res )
    } else {
        None
    }
}


//FIXME[rust/fat pointer cast]: make it ?Sized once it's supported by rust
#[doc(hidden)]
#[inline(always)]
pub fn uneraser_mut<GOT: 'static, EXP: 'static>(inp: &mut GOT ) -> Option<&mut EXP> {
    if TypeId::of::<GOT>() == TypeId::of::<EXP>() {
        //SAFE: the GOT type is exact the same as the EXP type,
        // the compiler just does not know this due to type erasure wrt.
        // generic types
        let res: &mut EXP = unsafe { mem::transmute::<&mut GOT, &mut EXP>(inp) };
        Some( res )
    } else {
        None
    }
}

//FIXME: only works if the rust compiler get's a bit more clever or a bit less (either is fine)
//#[inline(always)]
//pub fn uneraser<GOT: 'static, EXP: 'static>( inp: GOT ) -> Result<EXP, GOT> {
//    if TypeId::of::<GOT>() == TypeId::of::<EXP>() {
//        //SAFE: the GOT type is exact the same as the EXP type,
//        // the compiler just does not know this due to type erasure wrt.
//        // generic types
//        Ok( unsafe { mem::transmute::<GOT, EXP>( inp ) } )
//    } else {
//        Err( inp )
//    }
//}

//fn get_flat_byte_repr<T>(val: &T) -> Vec<u8> {
//    let count = mem::size_of::<T>();
//    let mut out = Vec::with_capacity(count);
//    let byte_ptr = val as *const T as *const u8;
//    for offset in 0..count {
//        out.push( unsafe {
//            *byte_ptr.offset(offset as isize)
//        })
//    }
//    out
//}



/// returns true if this a not first byte from a multi byte utf-8
///
/// This will return false:
/// - on all us-ascii  chars (as u8)
/// - on the first byte of a multi-byte utf-8 char
///
pub fn is_utf8_continuation_byte(b: u8) -> bool {
    // all additional bytes (and only them) in utf8 start with 0b10xxxxxx so while
    (b & 0b11000000) == 0b10000000
}

/// Faster insertion of byte slices into a byte vector.
pub fn vec_insert_bytes(target: &mut Vec<u8>, idx: usize, source: &[u8]) {
    use std::ptr::copy;

    let old_len = target.len();
    let insertion_len = source.len();
    let source_ptr = source.as_ptr();
    let insertion_point = unsafe { target.as_mut_ptr().offset(idx as isize) };
    let moved_data_len = old_len - idx;

    target.reserve(insertion_len);

    unsafe {
        copy(/*src*/insertion_point,
             /*dest*/insertion_point.offset(insertion_len as isize),
             /*count*/moved_data_len);

        copy(source_ptr, insertion_point, insertion_len);

        //3. set the new len for the vec
        target.set_len(old_len + insertion_len)
    }
}
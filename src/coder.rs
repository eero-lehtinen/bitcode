use crate::fast::VecImpl;
use std::mem::MaybeUninit;
use std::num::NonZeroUsize;

pub type Result<T> = std::result::Result<T, crate::Error>;

/// TODO pick different name because it aliases with [`crate::buffer::Buffer`].
pub trait Buffer {
    /// Convenience function for `collect_into`.
    fn collect(&mut self) -> Vec<u8> {
        let mut vec = vec![];
        self.collect_into(&mut vec);
        vec
    }

    /// Collects the buffer into a single `Vec<u8>`. This clears the buffer.
    fn collect_into(&mut self, out: &mut Vec<u8>);

    /// Reserves space for `additional` calls to `self.encode()`. Takes a [`NonZeroUsize`] to avoid
    /// useless calls.
    fn reserve(&mut self, additional: NonZeroUsize);
}

/// Iterators passed to [`Encoder::encode_vectored`] must have length <= this.
pub const MAX_VECTORED_CHUNK: usize = 64;

pub trait Encoder<T: ?Sized>: Buffer + Default {
    /// Returns a `VecImpl<T>` if `T` is a type that can be encoded by copying.
    #[inline(always)]
    fn as_primitive(&mut self) -> Option<&mut VecImpl<T>>
    where
        T: Sized,
    {
        None
    }

    /// Encodes a single value. Can't error since anything can be encoded.
    /// # Safety
    /// Can only encode `self.reserve(additional)` items.
    fn encode(&mut self, t: &T);

    /// Calls [`Self::encode`] once for every item in `i`. Only use this with **FAST** iterators
    /// since the iterator may be iterated multiple times.
    /// # Safety
    /// Can only encode `self.reserve(additional)` items.
    ///
    /// `i` must have an accurate `i.size_hint().1.unwrap()` that != 0 and is <= `MAX_VECTORED_CHUNK`.
    /// Currently, the non-map iterators that uphold these requirements are:
    /// - vec.rs
    /// - option.rs
    fn encode_vectored<'a>(&mut self, i: impl Iterator<Item = &'a T> + Clone)
    where
        T: 'a,
    {
        for t in i {
            self.encode(t);
        }
    }
}

pub trait View<'a> {
    /// Reads `length` items out of `input`, overwriting the view. If it returns `Ok`,
    /// `self.decode()` can be called called `length` times.
    fn populate(&mut self, input: &mut &'a [u8], length: usize) -> Result<()>;
}

/// One of [`Decoder::decode`] and [`Decoder::decode_in_place`] must be implemented or calling
/// either one will result in infinite recursion and a stack overflow.
pub trait Decoder<'a, T>: View<'a> + Default {
    /// Returns a pointer to the current element if it can be decoded by copying.
    #[inline(always)]
    fn as_primitive_ptr(&self) -> Option<*const u8> {
        None
    }

    /// Assuming [`Self::as_primitive_ptr`] returns `Some(ptr)`, this advances `ptr` by `n`.
    /// # Safety
    /// Can only decode `self.populate(_, length)` items.
    unsafe fn as_primitive_advance(&mut self, n: usize) {
        let _ = n;
        unreachable!();
    }

    /// Decodes a single value. Can't error since `View::populate` has already validated the input.
    /// Prefer decode for primitives (since it's simpler) and decode_in_place for array/struct/tuple.
    /// # Safety
    /// Can only decode `self.populate(_, length)` items.
    #[inline(always)]
    fn decode(&mut self) -> T {
        let mut out = MaybeUninit::uninit();
        self.decode_in_place(&mut out);
        unsafe { out.assume_init() }
    }

    /// [`Self::decode`] without redundant copies. Only downside is panics will leak the value.
    /// The only panics out of our control are Hash/Ord/PartialEq for BinaryHeap/BTreeMap/HashMap.
    /// E.g. if a user PartialEq panics we will leak some memory which is an acceptable tradeoff.
    /// # Safety
    /// Can only decode `self.populate(_, length)` items.
    #[inline(always)]
    fn decode_in_place(&mut self, out: &mut MaybeUninit<T>) {
        out.write(self.decode());
    }
}

#[doc(hidden)]
#[macro_export]
macro_rules! __private_uninit_field {
    ($uninit:ident.$field:tt:$field_ty:ty) => {
        unsafe {
            &mut *(std::ptr::addr_of_mut!((*$uninit.as_mut_ptr()).$field)
                as *mut std::mem::MaybeUninit<$field_ty>)
        }
    };
}
pub use __private_uninit_field as uninit_field;

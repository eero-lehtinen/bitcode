use crate::coder::{Buffer, Decoder, Encoder, Result, View, MAX_VECTORED_CHUNK};
use crate::derive::{Decode, Encode};
use crate::length::{LengthDecoder, LengthEncoder};
use std::collections::{BTreeSet, BinaryHeap, HashSet, LinkedList, VecDeque};
use std::hash::{BuildHasher, Hash};
use std::mem::MaybeUninit;
use std::num::NonZeroUsize;
use std::ptr::NonNull;

#[derive(Debug)]
pub struct VecEncoder<T: Encode> {
    // pub(crate) for arrayvec.rs
    pub(crate) lengths: LengthEncoder,
    pub(crate) elements: T::Encoder,
    vectored_impl: Option<NonNull<()>>,
}

// Can't derive since it would bound T: Default.
impl<T: Encode> Default for VecEncoder<T> {
    fn default() -> Self {
        Self {
            lengths: Default::default(),
            elements: Default::default(),
            vectored_impl: Default::default(),
        }
    }
}

impl<T: Encode> Buffer for VecEncoder<T> {
    fn collect_into(&mut self, out: &mut Vec<u8>) {
        self.lengths.collect_into(out);
        self.elements.collect_into(out);
    }

    fn reserve(&mut self, additional: NonZeroUsize) {
        self.lengths.reserve(additional);
        // We don't know the lengths of the vectors, so we can't reserve more.
    }
}

/// Copies `N` or `n` bytes from `src` to `dst` depending on if `src` lies within a memory page.
/// https://stackoverflow.com/questions/37800739/is-it-safe-to-read-past-the-end-of-a-buffer-within-the-same-page-on-x86-and-x64
/// Safety: Same as [`copy_nonoverlapping_unaligned`] but with the additional requirements that
/// `n != 0 && n <= N` and `dst` has room for a `[T; N]`.
/// Is a macro instead of an `#[inline(always)] fn` because it optimizes better.
macro_rules! unsafe_wild_copy {
    // pub unsafe fn wild_copy<T, const N: usize>(src: *const T, dst: *mut T, n: usize) {
    ([$T:ident; $N:ident], $src:ident, $dst:ident, $n:ident) => {
        debug_assert!($n != 0 && $n <= $N);

        let page_size = 4096;
        let read_size = std::mem::size_of::<[$T; $N]>();
        let within_page = $src as usize & (page_size - 1) < (page_size - read_size) && cfg!(all(
            // Miri doesn't like this.
            not(miri),
            // cargo fuzz's memory sanitizer complains about buffer overrun.
            // Without nightly we can't detect memory sanitizers, so we check debug_assertions.
            not(debug_assertions),
            // x86/x86_64/aarch64 all have min page size of 4096, so reading past the end of a non-empty
            // buffer won't page fault.
            any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")
        ));

        if within_page {
            std::ptr::write_unaligned($dst as *mut std::mem::MaybeUninit<[$T; $N]>,
                std::ptr::read_unaligned($src as *const std::mem::MaybeUninit<[$T; $N]>)
            );
        } else {
            #[cold]
            unsafe fn cold<T>(src: *const T, dst: *mut T, n: usize) {
                crate::derive::vec::copy_nonoverlapping_unaligned(src, dst, n);
            }
            cold($src, $dst, $n);
        }
    }
}
pub(crate) use unsafe_wild_copy;

/// Equivalent to `std::ptr::copy_nonoverlapping` but neither `src` nor `dst` has to be aligned.
/// Safety: Same as [`std::ptr::copy_nonoverlapping`], but without any alignment requirements.
#[inline(always)]
pub unsafe fn copy_nonoverlapping_unaligned<T>(src: *const T, dst: *mut T, n: usize) {
    std::ptr::copy_nonoverlapping(
        src as *const u8,
        dst as *mut u8,
        n * std::mem::size_of::<T>(),
    );
}

impl<T: Encode> VecEncoder<T> {
    /// Copy fixed size slices. Much faster than memcpy.
    #[inline(never)]
    fn encode_vectored_max_len<'a, I: Iterator<Item = &'a [T]> + Clone, const N: usize>(
        &mut self,
        i: I,
    ) where
        T: 'a,
    {
        unsafe {
            let primitives = self.elements.as_primitive().unwrap();
            primitives.reserve(i.size_hint().1.unwrap() * N);

            let mut dst = primitives.end_ptr();
            if self.lengths.encode_vectored_max_len::<_, N>(
                i.clone(),
                #[inline(always)]
                |s| {
                    let src = s.as_ptr();
                    let n = s.len();
                    // Safety: encode_vectored_max_len skips len == 0 and ensures len <= N.
                    // `dst` has enough space for `[T; N]` because we've reserved size_hint * N.
                    unsafe_wild_copy!([T; N], src, dst, n);
                    dst = dst.add(n);
                },
            ) {
                // Use fallback for impls that copy more than 64 bytes.
                let size = std::mem::size_of::<T>();
                self.vectored_impl = NonNull::new(match N {
                    1 if size <= 32 => Self::encode_vectored_max_len::<I, 2>,
                    2 if size <= 16 => Self::encode_vectored_max_len::<I, 4>,
                    4 if size <= 8 => Self::encode_vectored_max_len::<I, 8>,
                    8 if size <= 4 => Self::encode_vectored_max_len::<I, 16>,
                    16 if size <= 2 => Self::encode_vectored_max_len::<I, 32>,
                    32 if size <= 1 => Self::encode_vectored_max_len::<I, 64>,
                    _ => Self::encode_vectored_fallback::<I>,
                } as *mut ());
                let f: fn(&mut Self, i: I) = std::mem::transmute(self.vectored_impl);
                f(self, i);
                return;
            }
            primitives.set_end_ptr(dst);
        }
    }

    /// Fallback for when length > [`Self::encode_vectored_max_len`]'s max_len.
    #[inline(never)]
    fn encode_vectored_fallback<'a, I: Iterator<Item = &'a [T]>>(&mut self, i: I)
    where
        T: 'a,
    {
        let primitives = self.elements.as_primitive().unwrap();
        self.lengths.encode_vectored_fallback(i, |s| unsafe {
            let n = s.len();
            primitives.reserve(n);
            let ptr = primitives.end_ptr();
            copy_nonoverlapping_unaligned(s.as_ptr(), ptr, n);
            primitives.set_end_ptr(ptr.add(n));
        });
    }
}

impl<T: Encode> Encoder<[T]> for VecEncoder<T> {
    #[inline(always)]
    fn encode(&mut self, v: &[T]) {
        let n = v.len();
        self.lengths.encode(&n);

        if let Some(primitive) = self.elements.as_primitive() {
            primitive.reserve(n);
            unsafe {
                let ptr = primitive.end_ptr();
                copy_nonoverlapping_unaligned(v.as_ptr(), ptr, n);
                primitive.set_end_ptr(ptr.add(n));
            }
        } else if let Some(n) = NonZeroUsize::new(n) {
            self.elements.reserve(n);
            // Uses chunks to keep everything in the CPU cache. TODO pick optimal chunk size.
            for chunk in v.chunks(MAX_VECTORED_CHUNK) {
                self.elements.encode_vectored(chunk.iter());
            }
        }
    }

    #[inline(always)]
    fn encode_vectored<'a>(&mut self, i: impl Iterator<Item = &'a [T]> + Clone)
    where
        [T]: 'a,
    {
        if self.elements.as_primitive().is_some() {
            /// Convert impl trait to named generic type.
            #[inline(always)]
            fn inner<'a, T: Encode + 'a, I: Iterator<Item = &'a [T]> + Clone>(
                me: &mut VecEncoder<T>,
                i: I,
            ) {
                // We can't set this in the Default constructor because we don't have the type I.
                if me.vectored_impl.is_none() {
                    // Use match to avoid "use of generic parameter from outer function".
                    // Start at the pointer size (assumed to be 8 bytes) to not be wasteful.
                    me.vectored_impl = NonNull::new(match (8 / std::mem::size_of::<T>()).max(1) {
                        1 => VecEncoder::encode_vectored_max_len::<I, 1>,
                        2 => VecEncoder::encode_vectored_max_len::<I, 2>,
                        4 => VecEncoder::encode_vectored_max_len::<I, 4>,
                        8 => VecEncoder::encode_vectored_max_len::<I, 8>,
                        _ => unreachable!(),
                    } as *mut ());
                }
                let f: fn(&mut VecEncoder<T>, i: I) =
                    unsafe { std::mem::transmute(me.vectored_impl) };
                f(me, i);
            }
            inner(self, i);
        } else {
            for v in i {
                self.encode(v);
            }
        }
    }
}

#[derive(Debug)]
pub struct VecDecoder<'a, T: Decode<'a>> {
    // pub(crate) for arrayvec::ArrayVec.
    pub(crate) lengths: LengthDecoder<'a>,
    pub(crate) elements: T::Decoder,
}

// Can't derive since it would bound T: Default.
impl<'a, T: Decode<'a>> Default for VecDecoder<'a, T> {
    fn default() -> Self {
        Self {
            lengths: Default::default(),
            elements: Default::default(),
        }
    }
}

impl<'a, T: Decode<'a>> View<'a> for VecDecoder<'a, T> {
    fn populate(&mut self, input: &mut &'a [u8], length: usize) -> Result<()> {
        self.lengths.populate(input, length)?;
        self.elements.populate(input, self.lengths.length())
    }
}

macro_rules! encode_body {
    ($t:ty) => {
        #[inline(always)]
        fn encode(&mut self, v: &$t) {
            let n = v.len();
            self.lengths.encode(&n);
            if let Some(n) = NonZeroUsize::new(n) {
                self.elements.reserve(n);
                for v in v {
                    self.elements.encode(v);
                }
            }
        }
    };
}
// Faster on some collections.
macro_rules! encode_body_internal_iteration {
    ($t:ty) => {
        #[inline(always)]
        fn encode(&mut self, v: &$t) {
            let n = v.len();
            self.lengths.encode(&n);
            if let Some(n) = NonZeroUsize::new(n) {
                self.elements.reserve(n);
                v.iter().for_each(|v| self.elements.encode(v));
            }
        }
    };
}
macro_rules! decode_body {
    ($t:ty) => {
        #[inline(always)]
        fn decode(&mut self) -> $t {
            // - BTreeSet::from_iter is faster than BTreeSet::insert (see comment in map.rs).
            // - HashSet is about the same either way.
            // - Vec::from_iter is slower (so it doesn't use this).
            (0..self.lengths.decode())
                .map(|_| self.elements.decode())
                .collect()
        }
    };
}

impl<T: Encode> Encoder<Vec<T>> for VecEncoder<T> {
    #[inline(always)]
    fn encode(&mut self, v: &Vec<T>) {
        self.encode(v.as_slice());
    }

    #[inline(always)]
    fn encode_vectored<'a>(&mut self, i: impl Iterator<Item = &'a Vec<T>> + Clone)
    where
        Vec<T>: 'a,
    {
        self.encode_vectored(i.map(Vec::as_slice));
    }
}
impl<'a, T: Decode<'a>> Decoder<'a, Vec<T>> for VecDecoder<'a, T> {
    #[inline(always)]
    fn decode_in_place(&mut self, out: &mut MaybeUninit<Vec<T>>) {
        let length = self.lengths.decode();
        // Fast path, avoid memcpy and mutating len.
        if length == 0 {
            out.write(Vec::new());
            return;
        }

        let v = out.write(Vec::with_capacity(length));
        if let Some(primitive) = self.elements.as_primitive_ptr() {
            unsafe {
                copy_nonoverlapping_unaligned(primitive as *const T, v.as_mut_ptr(), length);
                self.elements.as_primitive_advance(length);
            }
        } else {
            let spare = v.spare_capacity_mut();
            for i in 0..length {
                let out = unsafe { spare.get_unchecked_mut(i) };
                self.elements.decode_in_place(out);
            }
        }
        unsafe { v.set_len(length) };
    }
}

impl<T: Encode> Encoder<BinaryHeap<T>> for VecEncoder<T> {
    encode_body!(BinaryHeap<T>); // When BinaryHeap::as_slice is stable use [T] impl.
}
impl<'a, T: Decode<'a> + Ord> Decoder<'a, BinaryHeap<T>> for VecDecoder<'a, T> {
    #[inline(always)]
    fn decode(&mut self) -> BinaryHeap<T> {
        let v: Vec<T> = self.decode();
        v.into()
    }
}

impl<T: Encode> Encoder<BTreeSet<T>> for VecEncoder<T> {
    encode_body!(BTreeSet<T>);
}
impl<'a, T: Decode<'a> + Ord> Decoder<'a, BTreeSet<T>> for VecDecoder<'a, T> {
    decode_body!(BTreeSet<T>);
}

impl<T: Encode, S> Encoder<HashSet<T, S>> for VecEncoder<T> {
    // Internal iteration is 1.6x faster. Interestingly this does not apply to HashMap<T, ()> which
    // I assume is due to HashSet::iter being implemented with HashMap::keys.
    encode_body_internal_iteration!(HashSet<T, S>);
}
impl<'a, T: Decode<'a> + Eq + Hash, S: BuildHasher + Default> Decoder<'a, HashSet<T, S>>
    for VecDecoder<'a, T>
{
    decode_body!(HashSet<T, S>);
}

impl<T: Encode> Encoder<LinkedList<T>> for VecEncoder<T> {
    encode_body!(LinkedList<T>);
}
impl<'a, T: Decode<'a>> Decoder<'a, LinkedList<T>> for VecDecoder<'a, T> {
    decode_body!(LinkedList<T>);
}

impl<T: Encode> Encoder<VecDeque<T>> for VecEncoder<T> {
    encode_body_internal_iteration!(VecDeque<T>); // Internal iteration is 10x faster.
}
impl<'a, T: Decode<'a>> Decoder<'a, VecDeque<T>> for VecDecoder<'a, T> {
    #[inline(always)]
    fn decode(&mut self) -> VecDeque<T> {
        let v: Vec<T> = self.decode();
        v.into()
    }
}

#[cfg(test)]
mod test {
    use std::collections::*;
    fn bench_data<T: FromIterator<u8>>() -> T {
        (0..=255).collect()
    }
    crate::bench_encode_decode!(
        btree_set: BTreeSet<_>,
        hash_set: HashSet<_>,
        linked_list: LinkedList<_>,
        vec: Vec<_>,
        vec_deque: VecDeque<_>
    );

    // BinaryHeap can't use bench_encode_decode because it doesn't implement PartialEq.
    #[bench]
    fn bench_binary_heap_decode(b: &mut test::Bencher) {
        type T = BinaryHeap<u8>;
        let data: T = bench_data();
        let encoded = crate::encode(&data);
        b.iter(|| {
            let decoded: T = crate::decode::<T>(&encoded).unwrap();
            debug_assert!(data.iter().eq(decoded.iter()));
            decoded
        })
    }
}

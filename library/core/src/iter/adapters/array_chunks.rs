use crate::iter::{Fuse, FusedIterator, Iterator, TrustedLen};
use crate::mem;
use crate::mem::MaybeUninit;
use crate::ops::{ControlFlow, Try};
use crate::ptr;

#[derive(Debug)]
struct Remainder<T, const N: usize> {
    array: [MaybeUninit<T>; N],
    init: usize,
}

impl<T, const N: usize> Remainder<T, N> {
    fn new() -> Self {
        Self { array: MaybeUninit::uninit_array(), init: 0 }
    }

    unsafe fn with_init(array: [MaybeUninit<T>; N], init: usize) -> Self {
        Self { array, init }
    }

    fn as_slice(&self) -> &[T] {
        debug_assert!(self.init <= N);
        // SAFETY: This raw slice will only contain the initialized objects
        // within the buffer.
        unsafe {
            let slice = self.array.get_unchecked(..self.init);
            MaybeUninit::slice_assume_init_ref(slice)
        }
    }

    fn as_mut_slice(&mut self) -> &mut [T] {
        debug_assert!(self.init <= N);
        // SAFETY: This raw slice will only contain the initialized objects
        // within the buffer.
        unsafe {
            let slice = self.array.get_unchecked_mut(..self.init);
            MaybeUninit::slice_assume_init_mut(slice)
        }
    }
}

impl<T, const N: usize> Clone for Remainder<T, N>
where
    T: Clone,
{
    fn clone(&self) -> Self {
        let mut new = Self::new();
        // SAFETY: The new array is the same size and `init` is always less than
        // or equal to `N`.
        let this = unsafe { new.array.get_unchecked_mut(..self.init) };
        MaybeUninit::write_slice_cloned(this, self.as_slice());
        new.init = self.init;
        new
    }
}

impl<T, const N: usize> Drop for Remainder<T, N> {
    fn drop(&mut self) {
        // SAFETY: This raw slice will only contain the initialized objects
        // within the buffer.
        unsafe { ptr::drop_in_place(self.as_mut_slice()) }
    }
}

/// An iterator over `N` elements of the iterator at a time.
///
/// The chunks do not overlap. If `N` does not divide the length of the
/// iterator, then the last up to `N-1` elements will be omitted.
///
/// This `struct` is created by the [`array_chunks`][Iterator::array_chunks]
/// method on [`Iterator`]. See its documentation for more.
#[derive(Debug, Clone)]
#[must_use = "iterators are lazy and do nothing unless consumed"]
#[unstable(feature = "iter_array_chunks", reason = "recently added", issue = "none")]
pub struct ArrayChunks<I: Iterator, const N: usize> {
    iter: Fuse<I>,
    remainder: Remainder<I::Item, N>,
}

impl<I, const N: usize> ArrayChunks<I, N>
where
    I: Iterator,
{
    pub(in crate::iter) fn new(iter: I) -> Self {
        assert!(N != 0, "chunk size must be non-zero");
        Self { iter: iter.fuse(), remainder: Remainder::new() }
    }

    /// Returns a reference to the remaining elements of the original iterator
    /// that are not going to be returned by this iterator. The returned slice
    /// has at most `N-1` elements.
    #[unstable(feature = "iter_array_chunks", reason = "recently added", issue = "none")]
    #[inline]
    pub fn remainder(&self) -> &[I::Item] {
        self.remainder.as_slice()
    }

    /// Returns a mutable reference to the remaining elements of the original
    /// iterator that are not going to be returned by this iterator. The
    /// returned slice has at most `N-1` elements.
    #[unstable(feature = "iter_array_chunks", reason = "recently added", issue = "none")]
    #[inline]
    pub fn remainder_mut(&mut self) -> &mut [I::Item] {
        self.remainder.as_mut_slice()
    }
}

#[unstable(feature = "iter_array_chunks", reason = "recently added", issue = "none")]
impl<I, const N: usize> Iterator for ArrayChunks<I, N>
where
    I: Iterator,
{
    type Item = [I::Item; N];

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let mut array = MaybeUninit::uninit_array();
        // SAFETY: `array` will still be valid if `guard` is dropped.
        let mut guard = unsafe { FrontGuard::new(&mut array) };

        for slot in array.iter_mut() {
            match self.iter.next() {
                Some(item) => {
                    slot.write(item);
                    guard.init += 1;
                }
                None => {
                    if guard.init > 0 {
                        let init = guard.init;
                        mem::forget(guard);
                        // SAFETY: `array` was initialized with `init` elements.
                        self.remainder = unsafe { Remainder::with_init(array, init) };
                    }
                    return None;
                }
            }
        }

        mem::forget(guard);
        // SAFETY: All elements of the array were populated in the loop above.
        Some(unsafe { MaybeUninit::array_assume_init(array) })
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let (lower, upper) = self.iter.size_hint();
        // Keep infinite iterator size hint lower bound as `usize::MAX`. This
        // is required to implement `TrustedLen`.
        if lower == usize::MAX {
            return (lower, upper);
        }
        (lower / N, upper.map(|n| n / N))
    }

    #[inline]
    fn count(self) -> usize {
        self.iter.count() / N
    }

    fn try_fold<B, F, R>(&mut self, init: B, mut f: F) -> R
    where
        Self: Sized,
        F: FnMut(B, Self::Item) -> R,
        R: Try<Output = B>,
    {
        let mut array = MaybeUninit::uninit_array();
        // SAFETY: `array` will still be valid if `guard` is dropped.
        let mut guard = unsafe { FrontGuard::new(&mut array) };

        let result = self.iter.try_fold(init, |mut acc, item| {
            // SAFETY: `init` starts at 0, increases by one each iteration and
            // is reset to 0 once it reaches N.
            unsafe { array.get_unchecked_mut(guard.init) }.write(item);
            guard.init += 1;
            if guard.init == N {
                guard.init = 0;
                let array = mem::replace(&mut array, MaybeUninit::uninit_array());
                // SAFETY: the condition above asserts that all elements are
                // initialized.
                let item = unsafe { MaybeUninit::array_assume_init(array) };
                acc = f(acc, item)?;
            }
            R::from_output(acc)
        });
        match result.branch() {
            ControlFlow::Continue(o) => {
                if guard.init > 0 {
                    let init = guard.init;
                    mem::forget(guard);
                    // SAFETY: `array` was initialized with `init` elements.
                    self.remainder = unsafe { Remainder::with_init(array, init) };
                }
                R::from_output(o)
            }
            ControlFlow::Break(r) => R::from_residual(r),
        }
    }

    fn fold<B, F>(self, init: B, mut f: F) -> B
    where
        Self: Sized,
        F: FnMut(B, Self::Item) -> B,
    {
        let mut array = MaybeUninit::uninit_array();
        // SAFETY: `array` will still be valid if `guard` is dropped.
        let mut guard = unsafe { FrontGuard::new(&mut array) };

        self.iter.fold(init, |mut acc, item| {
            // SAFETY: `init` starts at 0, increases by one each iteration and
            // is reset to 0 once it reaches N.
            unsafe { array.get_unchecked_mut(guard.init) }.write(item);
            guard.init += 1;
            if guard.init == N {
                guard.init = 0;
                let array = mem::replace(&mut array, MaybeUninit::uninit_array());
                // SAFETY: the condition above asserts that all elements are
                // initialized.
                let item = unsafe { MaybeUninit::array_assume_init(array) };
                acc = f(acc, item);
            }
            acc
        })
    }
}

/// A guard for an array where elements are filled from the left.
struct FrontGuard<T, const N: usize> {
    /// A pointer to the array that is being filled. We need to use a raw
    /// pointer here because of the lifetime issues in the fold implementations.
    ptr: *mut T,
    /// The number of *initialized* elements.
    init: usize,
}

impl<T, const N: usize> FrontGuard<T, N> {
    unsafe fn new(array: &mut [MaybeUninit<T>; N]) -> Self {
        Self { ptr: MaybeUninit::slice_as_mut_ptr(array), init: 0 }
    }
}

impl<T, const N: usize> Drop for FrontGuard<T, N> {
    fn drop(&mut self) {
        debug_assert!(self.init <= N);
        // SAFETY: This raw slice will only contain the initialized objects
        // within the buffer.
        unsafe {
            let slice = ptr::slice_from_raw_parts_mut(self.ptr, self.init);
            ptr::drop_in_place(slice);
        }
    }
}

#[unstable(feature = "iter_array_chunks", reason = "recently added", issue = "none")]
impl<I, const N: usize> DoubleEndedIterator for ArrayChunks<I, N>
where
    I: DoubleEndedIterator + ExactSizeIterator,
{
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        // We are iterating from the back we need to first handle the remainder.
        self.next_back_remainder()?;

        let mut array = MaybeUninit::uninit_array();
        // SAFETY: `array` will still be valid if `guard` is dropped.
        let mut guard = unsafe { BackGuard::new(&mut array) };

        for slot in array.iter_mut().rev() {
            slot.write(self.iter.next_back()?);
            guard.uninit -= 1;
        }

        mem::forget(guard);
        // SAFETY: All elements of the array were populated in the loop above.
        Some(unsafe { MaybeUninit::array_assume_init(array) })
    }

    fn try_rfold<B, F, R>(&mut self, init: B, mut f: F) -> R
    where
        Self: Sized,
        F: FnMut(B, Self::Item) -> R,
        R: Try<Output = B>,
    {
        // We are iterating from the back we need to first handle the remainder.
        if self.next_back_remainder().is_none() {
            return R::from_output(init);
        }

        let mut array = MaybeUninit::uninit_array();
        // SAFETY: `array` will still be valid if `guard` is dropped.
        let mut guard = unsafe { BackGuard::new(&mut array) };

        self.iter.try_rfold(init, |mut acc, item| {
            guard.uninit -= 1;
            // SAFETY: `uninit` starts at N, decreases by one each iteration and
            // is reset to N once it reaches 0.
            unsafe { array.get_unchecked_mut(guard.uninit) }.write(item);
            if guard.uninit == 0 {
                guard.uninit = N;
                let array = mem::replace(&mut array, MaybeUninit::uninit_array());
                // SAFETY: the condition above asserts that all elements are
                // initialized.
                let item = unsafe { MaybeUninit::array_assume_init(array) };
                acc = f(acc, item)?;
            }
            R::from_output(acc)
        })
    }

    fn rfold<B, F>(mut self, init: B, mut f: F) -> B
    where
        Self: Sized,
        F: FnMut(B, Self::Item) -> B,
    {
        // We are iterating from the back we need to first handle the remainder.
        if self.next_back_remainder().is_none() {
            return init;
        }

        let mut array = MaybeUninit::uninit_array();

        // SAFETY: `array` will still be valid if `guard` is dropped.
        let mut guard = unsafe { BackGuard::new(&mut array) };

        self.iter.rfold(init, |mut acc, item| {
            guard.uninit -= 1;
            // SAFETY: `uninit` starts at N, decreases by one each iteration and
            // is reset to N once it reaches 0.
            unsafe { array.get_unchecked_mut(guard.uninit) }.write(item);
            if guard.uninit == 0 {
                guard.uninit = N;
                let array = mem::replace(&mut array, MaybeUninit::uninit_array());
                // SAFETY: the condition above asserts that all elements are
                // initialized.
                let item = unsafe { MaybeUninit::array_assume_init(array) };
                acc = f(acc, item);
            }
            acc
        })
    }
}

impl<I, const N: usize> ArrayChunks<I, N>
where
    I: DoubleEndedIterator + ExactSizeIterator,
{
    #[inline]
    fn next_back_remainder(&mut self) -> Option<()> {
        // We use the `ExactSizeIterator` implementation of the underlying
        // iterator to know how many remaining elements there are.
        let rem = self.iter.len() % N;
        if rem == 0 {
            return Some(());
        }

        let mut array = MaybeUninit::uninit_array();

        // SAFETY: The array will still be valid if `guard` is dropped and
        // it is forgotten otherwise.
        let mut guard = unsafe { FrontGuard::new(&mut array) };

        // SAFETY: `rem` is in the range 1..N based on how it is calculated.
        for slot in unsafe { array.get_unchecked_mut(..rem) }.iter_mut() {
            slot.write(self.iter.next_back()?);
            guard.init += 1;
        }

        let init = guard.init;
        mem::forget(guard);
        // SAFETY: `array` was initialized with exactly `init` elements.
        self.remainder = unsafe {
            array.get_unchecked_mut(..init).reverse();
            Remainder::with_init(array, init)
        };
        Some(())
    }
}

/// A guard for an array where elements are filled from the right.
struct BackGuard<T, const N: usize> {
    /// A pointer to the array that is being filled. We need to use a raw
    /// pointer here because of the lifetime issues in the rfold implementations.
    ptr: *mut T,
    /// The number of *uninitialized* elements.
    uninit: usize,
}

impl<T, const N: usize> BackGuard<T, N> {
    unsafe fn new(array: &mut [MaybeUninit<T>; N]) -> Self {
        Self { ptr: MaybeUninit::slice_as_mut_ptr(array), uninit: N }
    }
}

impl<T, const N: usize> Drop for BackGuard<T, N> {
    fn drop(&mut self) {
        debug_assert!(self.uninit <= N);
        // SAFETY: This raw slice will only contain the initialized objects
        // within the buffer.
        unsafe {
            let ptr = self.ptr.offset(self.uninit as isize);
            let slice = ptr::slice_from_raw_parts_mut(ptr, N - self.uninit);
            ptr::drop_in_place(slice);
        }
    }
}

#[unstable(feature = "iter_array_chunks", reason = "recently added", issue = "none")]
impl<I, const N: usize> FusedIterator for ArrayChunks<I, N> where I: FusedIterator {}

#[unstable(feature = "iter_array_chunks", reason = "recently added", issue = "none")]
impl<I, const N: usize> ExactSizeIterator for ArrayChunks<I, N>
where
    I: ExactSizeIterator,
{
    #[inline]
    fn len(&self) -> usize {
        self.iter.len() / N
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.iter.len() / N == 0
    }
}

#[unstable(feature = "trusted_len", issue = "37572")]
unsafe impl<I, const N: usize> TrustedLen for ArrayChunks<I, N> where I: TrustedLen {}

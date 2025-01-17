//! Wrapper of the [array iterator API][iterator].
//!
//! This module exposes two iterators: [`NpySingleIter`] and [`NpyMultiIter`].
//!
//! As general recommendation, the usage of ndarray's facilities for iteration should be preferred:
//!
//! * They are more performant due to being transparent to the Rust compiler, using statically known dimensions
//!   without dynamic dispatch into NumPy's C implementation, c.f. [`ndarray::iter::Iter`].
//! * They are more flexible as to which parts of the array iterated in which order, c.f. [`ndarray::iter::Lanes`].
//! * They can zip up to six arrays together and operate on their elements using multiple threads, c.f. [`ndarray::Zip`].
//!
//! To safely use these types, extension functions should take [`PyReadonlyArray`] as arguments
//! which provide the [`as_array`][PyReadonlyArray::as_array] method to acquire an [`ndarray::ArrayView`].
//!
//! [iterator]: https://numpy.org/doc/stable/reference/c-api/iterator.html
#![deprecated(
    since = "0.16.0",
    note = "The wrappers of the array iterator API are deprecated, please use ndarray's iterators like `Lanes` and `Zip` instead."
)]
#![allow(missing_debug_implementations)]

use std::marker::PhantomData;
use std::os::raw::{c_char, c_int};
use std::ptr;

use ndarray::Dimension;
use pyo3::{PyErr, PyNativeType, PyResult, Python};

use crate::array::PyArrayDyn;
use crate::borrow::{PyReadonlyArray, PyReadwriteArray};
use crate::dtype::Element;
use crate::npyffi::{
    array::PY_ARRAY_API,
    npy_intp, npy_uint32,
    objects::NpyIter,
    types::{NPY_CASTING, NPY_ORDER},
    NPY_ITER_BUFFERED, NPY_ITER_COMMON_DTYPE, NPY_ITER_COPY_IF_OVERLAP, NPY_ITER_DELAY_BUFALLOC,
    NPY_ITER_DONT_NEGATE_STRIDES, NPY_ITER_GROWINNER, NPY_ITER_RANGED, NPY_ITER_READONLY,
    NPY_ITER_READWRITE, NPY_ITER_REDUCE_OK, NPY_ITER_REFS_OK, NPY_ITER_ZEROSIZE_OK,
};
use crate::sealed::Sealed;

/// Flags for constructing an iterator.
///
/// The meanings of these flags are defined in the [the NumPy documentation][iterator].
///
/// Note that some flags like `MultiIndex` and `ReadOnly` are directly represented
/// by the iterators types provided here.
///
/// [iterator]: https://numpy.org/doc/stable/reference/c-api/iterator.html#c.NpyIter_MultiNew
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NpyIterFlag {
    /// [`NPY_ITER_COMMON_DTYPE`](https://numpy.org/doc/stable/reference/c-api/iterator.html#c.NPY_ITER_COMMON_DTYPE)
    CommonDtype,
    /// [`NPY_ITER_REFS_OK`](https://numpy.org/doc/stable/reference/c-api/iterator.html#c.NPY_ITER_REFS_OK)
    RefsOk,
    /// [`NPY_ITER_ZEROSIZE_OK`](https://numpy.org/doc/stable/reference/c-api/iterator.html#c.NPY_ITER_ZEROSIZE_OK)
    ZerosizeOk,
    /// [`NPY_ITER_REDUCE_OK`](https://numpy.org/doc/stable/reference/c-api/iterator.html#c.NPY_ITER_REDUCE_OK)
    ReduceOk,
    /// [`NPY_ITER_RANGED`](https://numpy.org/doc/stable/reference/c-api/iterator.html#c.NPY_ITER_RANGED)
    Ranged,
    /// [`NPY_ITER_BUFFERED`](https://numpy.org/doc/stable/reference/c-api/iterator.html#c.NPY_ITER_BUFFERED)
    Buffered,
    /// [`NPY_ITER_GROWINNER`](https://numpy.org/doc/stable/reference/c-api/iterator.html#c.NPY_ITER_GROWINNER)
    GrowInner,
    /// [`NPY_ITER_DELAY_BUFALLOC`](https://numpy.org/doc/stable/reference/c-api/iterator.html#c.NPY_ITER_DELAY_BUFALLOC)
    DelayBufAlloc,
    /// [`NPY_ITER_DONT_NEGATE_STRIDES`](https://numpy.org/doc/stable/reference/c-api/iterator.html#c.NPY_ITER_DONT_NEGATE_STRIDES)
    DontNegateStrides,
    /// [`NPY_ITER_COPY_IF_OVERLAP`](https://numpy.org/doc/stable/reference/c-api/iterator.html#c.NPY_ITER_COPY_IF_OVERLAP)
    CopyIfOverlap,
    // CIndex,
    // FIndex,
    // MultiIndex,
    // ExternalLoop,
    // ReadWrite,
    // ReadOnly,
    // WriteOnly,
}

impl NpyIterFlag {
    fn to_c_enum(self) -> npy_uint32 {
        use NpyIterFlag::*;
        match self {
            CommonDtype => NPY_ITER_COMMON_DTYPE,
            RefsOk => NPY_ITER_REFS_OK,
            ZerosizeOk => NPY_ITER_ZEROSIZE_OK,
            ReduceOk => NPY_ITER_REDUCE_OK,
            Ranged => NPY_ITER_RANGED,
            Buffered => NPY_ITER_BUFFERED,
            GrowInner => NPY_ITER_GROWINNER,
            DelayBufAlloc => NPY_ITER_DELAY_BUFALLOC,
            DontNegateStrides => NPY_ITER_DONT_NEGATE_STRIDES,
            CopyIfOverlap => NPY_ITER_COPY_IF_OVERLAP,
        }
    }
}

/// Defines the sealed traits `IterMode` and `MultiIterMode`.
mod itermode {
    use super::*;

    /// A combinator type that represents the mode of an iterator.
    pub trait MultiIterMode: Sealed {
        #[doc(hidden)]
        type Pre: MultiIterMode;
        #[doc(hidden)]
        const FLAG: npy_uint32 = 0;
        #[doc(hidden)]
        fn flags() -> Vec<npy_uint32> {
            if Self::FLAG == 0 {
                Vec::new()
            } else {
                let mut res = Self::Pre::flags();
                res.push(Self::FLAG);
                res
            }
        }
    }

    impl MultiIterMode for () {
        type Pre = ();
    }

    /// Represents the iterator mode where the last array is readonly.
    pub struct RO<S: MultiIterMode>(PhantomData<S>);

    /// Represents the iterator mode where the last array is readwrite.
    pub struct RW<S: MultiIterMode>(PhantomData<S>);

    impl<S: MultiIterMode> Sealed for RO<S> {}

    impl<S: MultiIterMode> MultiIterMode for RO<S> {
        type Pre = S;
        const FLAG: npy_uint32 = NPY_ITER_READONLY;
    }

    impl<S: MultiIterMode> Sealed for RW<S> {}

    impl<S: MultiIterMode> MultiIterMode for RW<S> {
        type Pre = S;
        const FLAG: npy_uint32 = NPY_ITER_READWRITE;
    }

    /// Represents the iterator mode where at least two arrays are iterated.
    pub trait MultiIterModeWithManyArrays: MultiIterMode {}
    impl MultiIterModeWithManyArrays for RO<RO<()>> {}
    impl MultiIterModeWithManyArrays for RO<RW<()>> {}
    impl MultiIterModeWithManyArrays for RW<RO<()>> {}
    impl MultiIterModeWithManyArrays for RW<RW<()>> {}

    impl<S: MultiIterModeWithManyArrays> MultiIterModeWithManyArrays for RO<S> {}
    impl<S: MultiIterModeWithManyArrays> MultiIterModeWithManyArrays for RW<S> {}

    /// Iterator mode for single iterator
    pub trait IterMode: MultiIterMode {}
    /// Implies Readonly iterator.
    pub type Readonly = RO<()>;
    /// Implies Readwrite iterator.
    pub type ReadWrite = RW<()>;

    impl IterMode for RO<()> {}
    impl IterMode for RW<()> {}
}

pub use itermode::{
    IterMode, MultiIterMode, MultiIterModeWithManyArrays, ReadWrite, Readonly, RO, RW,
};

/// Builder of [`NpySingleIter`].
pub struct NpySingleIterBuilder<'py, T, I: IterMode> {
    flags: npy_uint32,
    array: &'py PyArrayDyn<T>,
    mode: PhantomData<I>,
}

impl<'py, T: Element> NpySingleIterBuilder<'py, T, Readonly> {
    /// Makes a new builder for a readonly iterator.
    pub fn readonly<D: Dimension>(array: &'py PyReadonlyArray<'_, T, D>) -> Self {
        Self {
            flags: NPY_ITER_READONLY,
            array: array.to_dyn(),
            mode: PhantomData,
        }
    }
}

impl<'py, T: Element> NpySingleIterBuilder<'py, T, ReadWrite> {
    /// Makes a new builder for a writable iterator.
    pub fn readwrite<D: Dimension>(array: &'py mut PyReadwriteArray<'_, T, D>) -> Self {
        Self {
            flags: NPY_ITER_READWRITE,
            array: array.to_dyn(),
            mode: PhantomData,
        }
    }
}

impl<'py, T: Element, I: IterMode> NpySingleIterBuilder<'py, T, I> {
    /// Applies a flag to this builder, returning `self`.
    #[must_use]
    pub fn set(mut self, flag: NpyIterFlag) -> Self {
        self.flags |= flag.to_c_enum();
        self
    }

    /// Creates an iterator from this builder.
    pub fn build(self) -> PyResult<NpySingleIter<'py, T, I>> {
        let array_ptr = self.array.as_array_ptr();
        let py = self.array.py();

        let iter_ptr = unsafe {
            PY_ARRAY_API.NpyIter_New(
                py,
                array_ptr,
                self.flags,
                NPY_ORDER::NPY_ANYORDER,
                NPY_CASTING::NPY_SAFE_CASTING,
                ptr::null_mut(),
            )
        };

        NpySingleIter::new(iter_ptr, py)
    }
}

/// An iterator over a single array, construced by [`NpySingleIterBuilder`].
///
/// The elements are access `&mut T` in case `readwrite` is used or
/// `&T` in case `readonly` is used.
///
/// # Example
///
/// You can use [`NpySingleIterBuilder::readwrite`] to get a mutable iterator.
///
/// ```
/// use numpy::pyo3::Python;
/// use numpy::{NpySingleIterBuilder, PyArray};
///
/// Python::with_gil(|py| {
///     let array = PyArray::arange(py, 0, 10, 1);
///     let mut array = array.readwrite();
///
///     let iter = NpySingleIterBuilder::readwrite(&mut array).build().unwrap();
///
///     for (i, elem) in iter.enumerate() {
///         assert_eq!(*elem, i as i64);
///
///         *elem = *elem * 2;  // Elements can be mutated.
///     }
/// });
/// ```
///
/// On the other hand, a readonly iterator requires an instance of [`PyReadonlyArray`].
///
/// ```
/// use numpy::pyo3::Python;
/// use numpy::{NpySingleIterBuilder, PyArray};
///
/// Python::with_gil(|py| {
///     let array = PyArray::arange(py, 0, 1, 10);
///     let array = array.readonly();
///
///     let iter = NpySingleIterBuilder::readonly(&array).build().unwrap();
///
///     for (i, elem) in iter.enumerate() {
///         assert_eq!(*elem, i as i64);
///     }
/// });
/// ```
pub struct NpySingleIter<'py, T, I> {
    iterator: ptr::NonNull<NpyIter>,
    iternext: unsafe extern "C" fn(*mut NpyIter) -> c_int,
    iter_size: npy_intp,
    dataptr: *mut *mut c_char,
    return_type: PhantomData<T>,
    mode: PhantomData<I>,
    py: Python<'py>,
}

impl<'py, T, I> NpySingleIter<'py, T, I> {
    fn new(iterator: *mut NpyIter, py: Python<'py>) -> PyResult<Self> {
        let mut iterator = match ptr::NonNull::new(iterator) {
            Some(iter) => iter,
            None => return Err(PyErr::fetch(py)),
        };

        let iternext = match unsafe {
            PY_ARRAY_API.NpyIter_GetIterNext(py, iterator.as_mut(), ptr::null_mut())
        } {
            Some(ptr) => ptr,
            None => return Err(PyErr::fetch(py)),
        };

        let dataptr = unsafe { PY_ARRAY_API.NpyIter_GetDataPtrArray(py, iterator.as_mut()) };
        if dataptr.is_null() {
            unsafe { PY_ARRAY_API.NpyIter_Deallocate(py, iterator.as_mut()) };
            return Err(PyErr::fetch(py));
        }

        let iter_size = unsafe { PY_ARRAY_API.NpyIter_GetIterSize(py, iterator.as_mut()) };

        Ok(Self {
            iterator,
            iternext,
            iter_size,
            dataptr,
            return_type: PhantomData,
            mode: PhantomData,
            py,
        })
    }

    fn iternext(&mut self) -> Option<*mut T> {
        if self.iter_size == 0 {
            None
        } else {
            // Note: This pointer is correct and doesn't need to be updated,
            // note that we're derefencing a **char into a *char casting to a *T
            // and then transforming that into a reference, the value that dataptr
            // points to is being updated by iternext to point to the next value.
            let ret = unsafe { *self.dataptr as *mut T };
            let empty = unsafe { (self.iternext)(self.iterator.as_mut()) } == 0;
            debug_assert_ne!(self.iter_size, 0);
            self.iter_size -= 1;
            debug_assert!(self.iter_size > 0 || empty);
            Some(ret)
        }
    }
}

impl<'py, T, I> Drop for NpySingleIter<'py, T, I> {
    fn drop(&mut self) {
        let _success = unsafe { PY_ARRAY_API.NpyIter_Deallocate(self.py, self.iterator.as_mut()) };
    }
}

impl<'py, T: 'py> Iterator for NpySingleIter<'py, T, Readonly> {
    type Item = &'py T;

    fn next(&mut self) -> Option<Self::Item> {
        self.iternext().map(|ptr| unsafe { &*ptr })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.len(), Some(self.len()))
    }
}

impl<'py, T: 'py> ExactSizeIterator for NpySingleIter<'py, T, Readonly> {
    fn len(&self) -> usize {
        self.iter_size as usize
    }
}

impl<'py, T: 'py> Iterator for NpySingleIter<'py, T, ReadWrite> {
    type Item = &'py mut T;

    fn next(&mut self) -> Option<Self::Item> {
        self.iternext().map(|ptr| unsafe { &mut *ptr })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.len(), Some(self.len()))
    }
}

impl<'py, T: 'py> ExactSizeIterator for NpySingleIter<'py, T, ReadWrite> {
    fn len(&self) -> usize {
        self.iter_size as usize
    }
}

/// Builder for [`NpyMultiIter`].
pub struct NpyMultiIterBuilder<'py, T, S: MultiIterMode> {
    flags: npy_uint32,
    arrays: Vec<&'py PyArrayDyn<T>>,
    structure: PhantomData<S>,
}

impl<'py, T: Element> Default for NpyMultiIterBuilder<'py, T, ()> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'py, T: Element> NpyMultiIterBuilder<'py, T, ()> {
    /// Creates a new builder.
    pub fn new() -> Self {
        Self {
            flags: 0,
            arrays: Vec::new(),
            structure: PhantomData,
        }
    }

    /// Applies a flag to this builder, returning `self`.
    #[must_use]
    pub fn set(mut self, flag: NpyIterFlag) -> Self {
        self.flags |= flag.to_c_enum();
        self
    }
}

impl<'py, T: Element, S: MultiIterMode> NpyMultiIterBuilder<'py, T, S> {
    /// Add a readonly array to the resulting iterator.
    pub fn add_readonly<D: Dimension>(
        mut self,
        array: &'py PyReadonlyArray<'_, T, D>,
    ) -> NpyMultiIterBuilder<'py, T, RO<S>> {
        self.arrays.push(array.to_dyn());
        NpyMultiIterBuilder {
            flags: self.flags,
            arrays: self.arrays,
            structure: PhantomData,
        }
    }

    /// Adds a writable array to the resulting iterator.
    pub fn add_readwrite<D: Dimension>(
        mut self,
        array: &'py mut PyReadwriteArray<'_, T, D>,
    ) -> NpyMultiIterBuilder<'py, T, RW<S>> {
        self.arrays.push(array.to_dyn());
        NpyMultiIterBuilder {
            flags: self.flags,
            arrays: self.arrays,
            structure: PhantomData,
        }
    }
}

impl<'py, T: Element, S: MultiIterModeWithManyArrays> NpyMultiIterBuilder<'py, T, S> {
    /// Creates an iterator from this builder.
    pub fn build(self) -> PyResult<NpyMultiIter<'py, T, S>> {
        let Self { flags, arrays, .. } = self;

        debug_assert!(arrays.len() <= i32::MAX as usize);
        debug_assert!(2 <= arrays.len());

        let mut opflags = S::flags();

        let py = arrays[0].py();

        let mut arrays = arrays.iter().map(|x| x.as_array_ptr()).collect::<Vec<_>>();

        let iter_ptr = unsafe {
            PY_ARRAY_API.NpyIter_MultiNew(
                py,
                arrays.len() as i32,
                arrays.as_mut_ptr(),
                flags,
                NPY_ORDER::NPY_ANYORDER,
                NPY_CASTING::NPY_SAFE_CASTING,
                opflags.as_mut_ptr(),
                ptr::null_mut(),
            )
        };

        NpyMultiIter::new(iter_ptr, py)
    }
}

/// An iterator over multiple arrays, construced by [`NpyMultiIterBuilder`].
///
/// [`NpyMultiIterBuilder::add_readwrite`] is used for adding a mutable component and
/// [`NpyMultiIterBuilder::add_readonly`] is used for adding an immutable one.
///
/// # Example
///
/// ```
/// use numpy::pyo3::Python;
/// use numpy::NpyMultiIterBuilder;
///
/// Python::with_gil(|py| {
///     let array1 = numpy::PyArray::arange(py, 0, 10, 1);
///     let array1 = array1.readonly();
///     let array2 = numpy::PyArray::arange(py, 10, 20, 1);
///     let mut array2 = array2.readwrite();
///     let array3 = numpy::PyArray::arange(py, 10, 30, 2);
///     let array3 = array3.readonly();
///
///     let iter = NpyMultiIterBuilder::new()
///             .add_readonly(&array1)
///             .add_readwrite(&mut array2)
///             .add_readonly(&array3)
///             .build()
///             .unwrap();
///
///     for (i, j, k) in iter {
///         assert_eq!(*i + *j, *k);
///         *j += *i + *k;  // Only the third element can be mutated.
///     }
/// });
/// ```
pub struct NpyMultiIter<'py, T, S: MultiIterModeWithManyArrays> {
    iterator: ptr::NonNull<NpyIter>,
    iternext: unsafe extern "C" fn(*mut NpyIter) -> c_int,
    iter_size: npy_intp,
    dataptr: *mut *mut c_char,
    marker: PhantomData<(T, S)>,
    py: Python<'py>,
}

impl<'py, T, S: MultiIterModeWithManyArrays> NpyMultiIter<'py, T, S> {
    fn new(iterator: *mut NpyIter, py: Python<'py>) -> PyResult<Self> {
        let mut iterator = match ptr::NonNull::new(iterator) {
            Some(ptr) => ptr,
            None => return Err(PyErr::fetch(py)),
        };

        let iternext = match unsafe {
            PY_ARRAY_API.NpyIter_GetIterNext(py, iterator.as_mut(), ptr::null_mut())
        } {
            Some(ptr) => ptr,
            None => return Err(PyErr::fetch(py)),
        };

        let dataptr = unsafe { PY_ARRAY_API.NpyIter_GetDataPtrArray(py, iterator.as_mut()) };
        if dataptr.is_null() {
            unsafe { PY_ARRAY_API.NpyIter_Deallocate(py, iterator.as_mut()) };
            return Err(PyErr::fetch(py));
        }

        let iter_size = unsafe { PY_ARRAY_API.NpyIter_GetIterSize(py, iterator.as_mut()) };

        Ok(Self {
            iterator,
            iternext,
            iter_size,
            dataptr,
            marker: PhantomData,
            py,
        })
    }
}

impl<'py, T, S: MultiIterModeWithManyArrays> Drop for NpyMultiIter<'py, T, S> {
    fn drop(&mut self) {
        let _success = unsafe { PY_ARRAY_API.NpyIter_Deallocate(self.py, self.iterator.as_mut()) };
    }
}

macro_rules! impl_multi_iter {
    ($structure: ty, $($ty: ty)+, $($ptr: ident)+, $expand: ident, $deref: expr) => {
        impl<'py, T: 'py> Iterator for NpyMultiIter<'py, T, $structure> {
            type Item = ($($ty,)+);

            fn next(&mut self) -> Option<Self::Item> {
                if self.iter_size == 0 {
                    None
                } else {
                    // Note: This pointer is correct and doesn't need to be updated,
                    // note that we're derefencing a **char into a *char casting to a *T
                    // and then transforming that into a reference, the value that dataptr
                    // points to is being updated by iternext to point to the next value.
                    let ($($ptr,)+) = unsafe { $expand::<T>(self.dataptr) };
                    let retval = Some(unsafe { $deref });
                    let empty = unsafe { (self.iternext)(self.iterator.as_mut()) } == 0;
                    debug_assert_ne!(self.iter_size, 0);
                    self.iter_size -= 1;
                    debug_assert!(self.iter_size > 0 || empty);
                    retval
                }
            }

            fn size_hint(&self) -> (usize, Option<usize>) {
                (self.len(), Some(self.len()))
            }
        }

        impl<'py, T: 'py> ExactSizeIterator for NpyMultiIter<'py, T, $structure> {
            fn len(&self) -> usize {
                self.iter_size as usize
            }
        }
    };
}

#[inline(always)]
unsafe fn expand2<T>(dataptr: *mut *mut c_char) -> (*mut T, *mut T) {
    (*dataptr as *mut T, *dataptr.offset(1) as *mut T)
}

#[inline(always)]
unsafe fn expand3<T>(dataptr: *mut *mut c_char) -> (*mut T, *mut T, *mut T) {
    (
        *dataptr as *mut T,
        *dataptr.offset(1) as *mut T,
        *dataptr.offset(2) as *mut T,
    )
}

impl_multi_iter!(RO<RO<()>>, &'py T &'py T, a b, expand2, (&*a, &*b));
impl_multi_iter!(RO<RW<()>>, &'py mut T &'py T, a b, expand2, (&mut *a, &*b));
impl_multi_iter!(RW<RO<()>>, &'py T &'py mut T, a b, expand2, (&*a, &mut *b));
impl_multi_iter!(RW<RW<()>>, &'py mut T &'py mut T, a b, expand2, (&mut *a, &mut *b));
impl_multi_iter!(RO<RO<RO<()>>>, &'py T &'py T &'py T, a b c, expand3, (&*a, &*b, &*c));
impl_multi_iter!(RO<RO<RW<()>>>, &'py mut T &'py T &'py T, a b c, expand3, (&mut *a, &*b, &*c));
impl_multi_iter!(RO<RW<RO<()>>>, &'py T &'py mut T &'py T, a b c, expand3, (&*a, &mut *b, &*c));
impl_multi_iter!(RW<RO<RO<()>>>, &'py T &'py T &'py mut T, a b c, expand3, (&*a, &*b, &mut *c));
impl_multi_iter!(RO<RW<RW<()>>>, &'py mut T &'py mut T &'py T, a b c, expand3, (&mut *a, &mut *b, &*c));
impl_multi_iter!(RW<RO<RW<()>>>, &'py mut T &'py T &'py mut T, a b c, expand3, (&mut *a, &*b, &mut *c));
impl_multi_iter!(RW<RW<RO<()>>>, &'py T &'py mut T &'py mut T, a b c, expand3, (&*a, &mut *b, &mut *c));
impl_multi_iter!(RW<RW<RW<()>>>, &'py mut T &'py mut T &'py mut T, a b c, expand3, (&mut *a, &mut *b, &mut *c));

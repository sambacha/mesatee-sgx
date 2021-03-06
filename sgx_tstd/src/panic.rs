// Copyright (C) 2017-2019 Baidu, Inc. All Rights Reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions
// are met:
//
//  * Redistributions of source code must retain the above copyright
//    notice, this list of conditions and the following disclaimer.
//  * Redistributions in binary form must reproduce the above copyright
//    notice, this list of conditions and the following disclaimer in
//    the documentation and/or other materials provided with the
//    distribution.
//  * Neither the name of Baidu, Inc., nor the names of its
//    contributors may be used to endorse or promote products derived
//    from this software without specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
// "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
// LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
// A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
// OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
// SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
// LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
// DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
// THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
// (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
// OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

//! Panic support in the standard library

use crate::panicking;
use crate::thread::Result;
use core::any::Any;
use core::cell::UnsafeCell;
use crate::collections;
use core::fmt;
use core::pin::Pin;
use core::ops::{Deref, DerefMut, Fn};
use core::ptr::{Unique, NonNull};
use core::sync::atomic;
use core::future::Future;
use core::task::{Context, Poll};
use alloc_crate::boxed::Box;
use alloc_crate::rc::Rc;
use alloc_crate::sync::Arc;

pub use crate::panicking::set_panic_handler;
pub use core::panic::{PanicInfo, Location};
/// A marker trait which represents "panic safe" types in Rust.
///
/// This trait is implemented by default for many types and behaves similarly in
/// terms of inference of implementation to the [`Send`] and [`Sync`] traits. The
/// purpose of this trait is to encode what types are safe to cross a [`catch_unwind`]
/// boundary with no fear of unwind safety.
///
/// ## What is unwind safety?
///
/// In Rust a function can "return" early if it either panics or calls a
/// function which transitively panics. This sort of control flow is not always
/// anticipated, and has the possibility of causing subtle bugs through a
/// combination of two critical components:
///
/// 1. A data structure is in a temporarily invalid state when the thread
///    panics.
/// 2. This broken invariant is then later observed.
///
/// Typically in Rust, it is difficult to perform step (2) because catching a
/// panic involves either spawning a thread (which in turns makes it difficult
/// to later witness broken invariants) or using the `catch_unwind` function in this
/// module. Additionally, even if an invariant is witnessed, it typically isn't a
/// problem in Rust because there are no uninitialized values (like in C or C++).
///
/// It is possible, however, for **logical** invariants to be broken in Rust,
/// which can end up causing behavioral bugs. Another key aspect of unwind safety
/// in Rust is that, in the absence of `unsafe` code, a panic cannot lead to
/// memory unsafety.
///
/// That was a bit of a whirlwind tour of unwind safety, but for more information
/// about unwind safety and how it applies to Rust, see an [associated RFC][rfc].
///
/// [rfc]: https://github.com/rust-lang/rfcs/blob/master/text/1236-stabilize-catch-panic.md
///
/// ## What is `UnwindSafe`?
///
/// Now that we've got an idea of what unwind safety is in Rust, it's also
/// important to understand what this trait represents. As mentioned above, one
/// way to witness broken invariants is through the `catch_unwind` function in this
/// module as it allows catching a panic and then re-using the environment of
/// the closure.
///
/// Simply put, a type `T` implements `UnwindSafe` if it cannot easily allow
/// witnessing a broken invariant through the use of `catch_unwind` (catching a
/// panic). This trait is an auto trait, so it is automatically implemented for
/// many types, and it is also structurally composed (e.g., a struct is unwind
/// safe if all of its components are unwind safe).
///
/// Note, however, that this is not an unsafe trait, so there is not a succinct
/// contract that this trait is providing. Instead it is intended as more of a
/// "speed bump" to alert users of `catch_unwind` that broken invariants may be
/// witnessed and may need to be accounted for.
///
/// ## Who implements `UnwindSafe`?
///
/// Types such as `&mut T` and `&RefCell<T>` are examples which are **not**
/// unwind safe. The general idea is that any mutable state which can be shared
/// across `catch_unwind` is not unwind safe by default. This is because it is very
/// easy to witness a broken invariant outside of `catch_unwind` as the data is
/// simply accessed as usual.
///
/// Types like `&SgxMutex<T>`, however, are unwind safe because they implement
/// poisoning by default. They still allow witnessing a broken invariant, but
/// they already provide their own "speed bumps" to do so.
///
/// ## When should `UnwindSafe` be used?
///
/// It is not intended that most types or functions need to worry about this trait.
/// It is only used as a bound on the `catch_unwind` function and as mentioned
/// above, the lack of `unsafe` means it is mostly an advisory. The
/// [`AssertUnwindSafe`] wrapper struct can be used to force this trait to be
/// implemented for any closed over variables passed to `catch_unwind`.
///
/// [`AssertUnwindSafe`]: ./struct.AssertUnwindSafe.html
pub auto trait UnwindSafe {}

/// A marker trait representing types where a shared reference is considered
/// unwind safe.
///
/// This trait is namely not implemented by [`UnsafeCell`], the root of all
/// interior mutability.
///
/// This is a "helper marker trait" used to provide impl blocks for the
/// [`UnwindSafe`] trait, for more information see that documentation.
///
/// [`UnsafeCell`]: ../cell/struct.UnsafeCell.html
/// [`UnwindSafe`]: ./trait.UnwindSafe.html
pub auto trait RefUnwindSafe {}

/// A simple wrapper around a type to assert that it is unwind safe.
///
/// When using [`catch_unwind`] it may be the case that some of the closed over
/// variables are not unwind safe. For example if `&mut T` is captured the
/// compiler will generate a warning indicating that it is not unwind safe. It
/// may not be the case, however, that this is actually a problem due to the
/// specific usage of [`catch_unwind`] if unwind safety is specifically taken into
/// account. This wrapper struct is useful for a quick and lightweight
/// annotation that a variable is indeed unwind safe.
///
/// [`catch_unwind`]: ./fn.catch_unwind.html
pub struct AssertUnwindSafe<T>(
    pub T
);

// Implementations of the `UnwindSafe` trait:
//
// * By default everything is unwind safe
// * pointers T contains mutability of some form are not unwind safe
// * Unique, an owning pointer, lifts an implementation
// * Types like Mutex/RwLock which are explicitly poisoned are unwind safe
// * Our custom AssertUnwindSafe wrapper is indeed unwind safe

impl<T: ?Sized> !UnwindSafe for &mut T {}
impl<T: RefUnwindSafe + ?Sized> UnwindSafe for &T {}
impl<T: RefUnwindSafe + ?Sized> UnwindSafe for *const T {}
impl<T: RefUnwindSafe + ?Sized> UnwindSafe for *mut T {}
impl<T: UnwindSafe + ?Sized> UnwindSafe for Unique<T> {}
impl<T: RefUnwindSafe + ?Sized> UnwindSafe for NonNull<T> {}
impl<T> UnwindSafe for AssertUnwindSafe<T> {}

// not covered via the Shared impl above b/c the inner contents use
// Cell/AtomicUsize, but the usage here is unwind safe so we can lift the
// impl up one level to Arc/Rc itself
impl<T: RefUnwindSafe + ?Sized> UnwindSafe for Rc<T> {}
impl<T: RefUnwindSafe + ?Sized> UnwindSafe for Arc<T> {}

// Pretty simple implementations for the `RefUnwindSafe` marker trait,
// basically just saying that `UnsafeCell` is the
// only thing which doesn't implement it (which then transitively applies to
// everything else).

impl<T: ?Sized> !RefUnwindSafe for UnsafeCell<T> {}
impl<T> RefUnwindSafe for AssertUnwindSafe<T> {}

#[cfg(target_has_atomic = "ptr")]
impl RefUnwindSafe for atomic::AtomicIsize {}
#[cfg(target_has_atomic = "8")]
impl RefUnwindSafe for atomic::AtomicI8 {}
#[cfg(target_has_atomic = "16")]
impl RefUnwindSafe for atomic::AtomicI16 {}
#[cfg(target_has_atomic = "32")]
impl RefUnwindSafe for atomic::AtomicI32 {}
#[cfg(target_has_atomic = "64")]
impl RefUnwindSafe for atomic::AtomicI64 {}
#[cfg(target_has_atomic = "128")]
impl RefUnwindSafe for atomic::AtomicI128 {}

#[cfg(target_has_atomic = "ptr")]
impl RefUnwindSafe for atomic::AtomicUsize {}
#[cfg(target_has_atomic = "8")]
impl RefUnwindSafe for atomic::AtomicU8 {}
#[cfg(target_has_atomic = "16")]
impl RefUnwindSafe for atomic::AtomicU16 {}
#[cfg(target_has_atomic = "32")]
impl RefUnwindSafe for atomic::AtomicU32 {}
#[cfg(target_has_atomic = "64")]
impl RefUnwindSafe for atomic::AtomicU64 {}
#[cfg(target_has_atomic = "128")]
impl RefUnwindSafe for atomic::AtomicU128 {}

#[cfg(target_has_atomic = "8")]
impl RefUnwindSafe for atomic::AtomicBool {}

#[cfg(target_has_atomic = "ptr")]
impl<T> RefUnwindSafe for atomic::AtomicPtr<T> {}

impl<K, V, S> UnwindSafe for collections::HashMap<K, V, S>
    where K: UnwindSafe, V: UnwindSafe, S: UnwindSafe {}

impl<T> Deref for AssertUnwindSafe<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> DerefMut for AssertUnwindSafe<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

impl<R, F: FnOnce() -> R> FnOnce<()> for AssertUnwindSafe<F> {
    type Output = R;

    extern "rust-call" fn call_once(self, _args: ()) -> R {
        (self.0)()
    }
}

impl<T: fmt::Debug> fmt::Debug for AssertUnwindSafe<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AssertUnwindSafe")
            .field(&self.0)
            .finish()
    }
}

impl<F: Future> Future for AssertUnwindSafe<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let pinned_field = unsafe { Pin::map_unchecked_mut(self, |x| &mut x.0) };
        F::poll(pinned_field, cx)
    }
}

/// Invokes a closure, capturing the cause of an unwinding panic if one occurs.
///
/// This function will return `Ok` with the closure's result if the closure
/// does not panic, and will return `Err(cause)` if the closure panics. The
/// `cause` returned is the object with which panic was originally invoked.
///
/// It is currently undefined behavior to unwind from Rust code into foreign
/// code, so this function is particularly useful when Rust is called from
/// another language (normally C). This can run arbitrary Rust code, capturing a
/// panic and allowing a graceful handling of the error.
///
/// It is **not** recommended to use this function for a general try/catch
/// mechanism. The [`Result`] type is more appropriate to use for functions that
/// can fail on a regular basis. Additionally, this function is not guaranteed
/// to catch all panics, see the "Notes" section below.
///
/// [`Result`]: ../result/enum.Result.html
///
/// The closure provided is required to adhere to the [`UnwindSafe`] trait to ensure
/// that all captured variables are safe to cross this boundary. The purpose of
/// this bound is to encode the concept of [exception safety][rfc] in the type
/// system. Most usage of this function should not need to worry about this
/// bound as programs are naturally unwind safe without `unsafe` code. If it
/// becomes a problem the [`AssertUnwindSafe`] wrapper struct can be used to quickly
/// assert that the usage here is indeed unwind safe.
///
/// [`AssertUnwindSafe`]: ./struct.AssertUnwindSafe.html
/// [`UnwindSafe`]: ./trait.UnwindSafe.html
///
/// [rfc]: https://github.com/rust-lang/rfcs/blob/master/text/1236-stabilize-catch-panic.md
///
/// # Notes
///
/// Note that this function **may not catch all panics** in Rust. A panic in
/// Rust is not always implemented via unwinding, but can be implemented by
/// aborting the process as well. This function *only* catches unwinding panics,
/// not those that abort the process.
///
pub fn catch_unwind<F: FnOnce() -> R + UnwindSafe, R>(f: F) -> Result<R> {
    unsafe {
        panicking::r#try(f)
    }
}

/// Triggers a panic without invoking the panic hook.
///
/// This is designed to be used in conjunction with [`catch_unwind`] to, for
/// example, carry a panic across a layer of C code.
///
/// [`catch_unwind`]: ./fn.catch_unwind.html
///
/// # Notes
///
/// Note that panics in Rust are not always implemented via unwinding, but they
/// may be implemented by aborting the process. If this function is called when
/// panics are implemented this way then this function will abort the process,
/// not trigger an unwind.
///
pub fn resume_unwind(payload: Box<dyn Any + Send>) -> ! {
    panicking::update_count_then_panic(payload)
}

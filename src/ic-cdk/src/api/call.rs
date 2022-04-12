//! APIs to make and manage calls in the canister.
use crate::api::{ic0, trap};
use crate::export::Principal;
use candid::utils::{ArgumentDecoder, ArgumentEncoder};
use candid::{decode_args, encode_args, write_args, CandidType};
use serde::ser::Error;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll, Waker};

#[cfg(all(
    target_arch = "wasm32-unknown-unknown",
    not(target_feature = "atomics")
))]
#[allow(dead_code)]
mod rc {
    use std::cell::{RefCell, RefMut};
    use std::future::Future;
    use std::pin::Pin;
    use std::rc::Rc;
    use std::task::{Context, Poll};

    pub(crate) type InnerCell<T> = RefCell<T>;

    /// A reference counted cell. This is a specific implementation that is
    /// both Send and Sync, but does not rely on Mutex and Arc in WASM as
    /// the actual implementation of Mutex can break in async flows.
    pub(crate) struct WasmCell<T>(Rc<InnerCell<T>>);

    /// In order to be able to have an async method that returns the
    /// result of a call to another canister, we need that result to
    /// be Send + Sync, but Rc and RefCell are not.
    ///
    /// Since inside a canister there isn't actual concurrent access to
    /// the referenced cell or the reference counted container, it is
    /// safe to force these to be Send/Sync.
    unsafe impl<T> Send for WasmCell<T> {}
    unsafe impl<T> Sync for WasmCell<T> {}

    impl<T> WasmCell<T> {
        pub fn new(val: T) -> Self {
            WasmCell(Rc::new(InnerCell::new(val)))
        }
        pub fn into_raw(self) -> *const InnerCell<T> {
            Rc::into_raw(self.0)
        }
        /// # Safety
        /// The pointer must have been created with [`into_raw`].
        pub unsafe fn from_raw(ptr: *const InnerCell<T>) -> Self {
            Self(Rc::from_raw(ptr))
        }
        pub fn borrow_mut(&self) -> RefMut<'_, T> {
            self.0.borrow_mut()
        }
        pub fn as_ptr(&self) -> *const InnerCell<T> {
            self.0.as_ptr() as *const _
        }
    }

    impl<O, T: Future<Output = O>> Future for WasmCell<T> {
        type Output = O;

        #[allow(unused_mut)]
        fn poll(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
            unsafe { Pin::new_unchecked(&mut *self.0.borrow_mut()) }.poll(ctx)
        }
    }

    impl<T> Clone for WasmCell<T> {
        fn clone(&self) -> Self {
            WasmCell(Rc::clone(&self.0))
        }
    }
}

#[cfg(all(target_arch = "wasm32-unknown-unknown", target_feature = "atomics"))]
compile_error!("The CDK is not usable when targeting atomic WASM.");

#[cfg(not(target_arch = "wasm32-unknown-unknown"))]
#[allow(dead_code)]
mod rc {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::task::{Context, Poll};

    pub(crate) type InnerCell<T> = Mutex<T>;

    /// A reference counted cell. This is a specific implementation that is
    /// both Send and Sync, but does not rely on Mutex and Arc in WASM as
    /// the actual implementation of Mutex can break in async flows.
    ///
    /// The RefCell is for
    pub(crate) struct WasmCell<T>(Arc<InnerCell<T>>);

    impl<T> WasmCell<T> {
        pub fn new(val: T) -> Self {
            WasmCell(Arc::new(InnerCell::new(val)))
        }
        pub fn into_raw(self) -> *const InnerCell<T> {
            Arc::into_raw(self.0)
        }
        /// # Safety
        /// The pointer must have been created with [`into_raw`].
        pub unsafe fn from_raw(ptr: *const InnerCell<T>) -> Self {
            // SAFETY: If the pointer was created from into_raw, it internally was created from Arc::into_raw.
            Self(unsafe { Arc::from_raw(ptr) })
        }
        pub fn borrow_mut(&self) -> MutexGuard<'_, T> {
            self.0.lock().unwrap()
        }
        pub fn as_ptr(&self) -> *const InnerCell<T> {
            Arc::<_>::as_ptr(&self.0)
        }
    }

    impl<O, T: Future<Output = O>> Future for WasmCell<T> {
        type Output = O;

        #[allow(unused_mut)]
        fn poll(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
            // SAFETY: this is a projection of self, which is pinned
            unsafe { Pin::new_unchecked(&mut *self.0.lock().unwrap()) }.poll(ctx)
        }
    }

    impl<T> Clone for WasmCell<T> {
        fn clone(&self) -> Self {
            WasmCell(Arc::clone(&self.0))
        }
    }
}

use rc::{InnerCell, WasmCell};

/// Rejection code from calling another canister.
///
/// These can be obtained either using `reject_code()` or `reject_result()`.
#[allow(missing_docs)]
#[repr(i32)]
#[derive(Debug, Clone, Copy)]
pub enum RejectionCode {
    NoError = 0,

    SysFatal = 1,
    SysTransient = 2,
    DestinationInvalid = 3,
    CanisterReject = 4,
    CanisterError = 5,

    Unknown,
}

impl From<i32> for RejectionCode {
    fn from(code: i32) -> Self {
        match code {
            0 => RejectionCode::NoError,
            1 => RejectionCode::SysFatal,
            2 => RejectionCode::SysTransient,
            3 => RejectionCode::DestinationInvalid,
            4 => RejectionCode::CanisterReject,
            5 => RejectionCode::CanisterError,
            _ => RejectionCode::Unknown,
        }
    }
}

impl From<u32> for RejectionCode {
    fn from(code: u32) -> Self {
        RejectionCode::from(code as i32)
    }
}

/// The result of a Call.
///
/// Errors on the IC have two components; a Code and a message associated with it.
pub type CallResult<R> = Result<R, (RejectionCode, String)>;

// Internal state for the Future when sending a call.
struct CallFutureState<R: serde::de::DeserializeOwned> {
    result: Option<CallResult<R>>,
    waker: Option<Waker>,
}

struct CallFuture<R: serde::de::DeserializeOwned> {
    // We basically use Rc instead of Arc (since we're single threaded), and use
    // RefCell instead of Mutex (because we cannot lock in WASM).
    state: rc::WasmCell<CallFutureState<R>>,
}

impl<R: serde::de::DeserializeOwned> Future for CallFuture<R> {
    type Output = Result<R, (RejectionCode, String)>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let self_ref = Pin::into_ref(self);

        let mut state = self_ref.state.borrow_mut();

        if let Some(result) = state.result.take() {
            Poll::Ready(result)
        } else {
            state.waker = Some(context.waker().clone());
            Poll::Pending
        }
    }
}

/// The callback from IC dereferences the future from a raw pointer, assigns the
/// result and calls the waker. We cannot use a closure here because we pass raw
/// pointers to the System and back.
///
/// # Safety
///
/// This function must only be passed to the IC with a pointer from WasmCell::into_raw as userdata.
unsafe fn callback(state_ptr: *const InnerCell<CallFutureState<Vec<u8>>>) {
    // SAFETY: This function is only ever called by the IC, and we only ever pass a WasmCell as userdata.
    let state = unsafe { WasmCell::from_raw(state_ptr) };
    // Make sure to un-borrow_mut the state.
    {
        state.borrow_mut().result = Some(match reject_code() {
            RejectionCode::NoError => Ok(arg_data_raw()),
            n => Err((n, reject_message())),
        });
    }
    let w = state.borrow_mut().waker.take();
    if let Some(waker) = w {
        // This is all to protect this little guy here which will call the poll() which
        // borrow_mut() the state as well. So we need to be careful to not double-borrow_mut.
        waker.wake()
    }
}

/// This function is called when [callback] was just called with the same parameter, and trapped.
/// We can't guarantee internal consistency at this point, but we can at least e.g. drop mutex guards.
/// Waker is a very opaque API, so the best we can do is set a global flag and proceed normally.
///
/// # Safety
///
/// This function must only be passed to the IC with a pointer from WasmCell::into_raw as userdata.
unsafe fn cleanup(state_ptr: *const InnerCell<CallFutureState<Vec<u8>>>) {
    // SAFETY: This function is only ever called by the IC, and we only ever pass a WasmCell as userdata.
    let state = unsafe { WasmCell::from_raw(state_ptr) };
    // We set the call result, even though it won't be read on the default executor, because we can't guarantee it was called on our executor.
    // None of these calls trap - the rollback from the previous trap ensures that the Mutex is not in a poisoned state.
    {
        state.borrow_mut().result = Some(match reject_code() {
            RejectionCode::NoError => Ok(arg_data_raw()),
            n => Err((n, reject_message())),
        });
    }
    let w = state.borrow_mut().waker.take();
    if let Some(waker) = w {
        // Flag that we do not want to actually wake the task - we want to drop it *without* executing it.
        crate::futures::CLEANUP.store(true, Ordering::Relaxed);
        waker.wake();
        crate::futures::CLEANUP.store(false, Ordering::Relaxed);
    }
}

/// Similar to `call`, but without serialization.
pub fn call_raw(
    id: Principal,
    method: &str,
    args_raw: &[u8],
    payment: u64,
) -> impl Future<Output = CallResult<Vec<u8>>> {
    call_raw_internal(id, method, args_raw, move || {
        if payment > 0 {
            // SAFETY: ic0.call_cycles_add is always safe to call.
            unsafe {
                // This is called as part of the call_new lifecycle, and so will not trap.
                ic0::call_cycles_add(payment as i64);
            }
        }
    })
}

/// Similar to `call128`, but without serialization.
pub fn call_raw128(
    id: Principal,
    method: &str,
    args_raw: &[u8],
    payment: u128,
) -> impl Future<Output = CallResult<Vec<u8>>> {
    call_raw_internal(id, method, args_raw, move || {
        if payment > 0 {
            // SAFETY: ic0.call_cycles_add128 is always safe to call.
            unsafe {
                let high = (payment >> 64) as u64;
                let low = (payment & u64::MAX as u128) as u64;
                // This is called as part of the call_new lifecycle, and so will not trap.
                ic0::call_cycles_add128(high as i64, low as i64);
            }
        }
    })
}

fn call_raw_internal(
    id: Principal,
    method: &str,
    args_raw: &[u8],
    payment_func: impl FnOnce(),
) -> impl Future<Output = CallResult<Vec<u8>>> {
    let callee = id.as_slice();
    let state = WasmCell::new(CallFutureState {
        result: None,
        waker: None,
    });
    let state_ptr = WasmCell::into_raw(state.clone());
    // SAFETY:
    // `callee`, being &[u8], is a readable sequence of bytes and therefore can be passed to ic0.call_new.
    // `method`, being &str, is a readable sequence of bytes and therefore can be passed to ic0.call_new.
    // `callback` is a function with signature (env : i32) -> () and therefore can be called as both reply and reject fn for ic0.call_new.
    // `state_ptr` is a pointer created via WasmCell::into_raw, and can therefore be passed as the userdata for `callback`.
    // `args`, being a &[u8], is a readable sequence of bytes and therefore can be passed to ic0.call_data_append.
    // `cleanup` is a function with signature (env : i32) -> () and therefore can be called as a cleanup fn for ic0.call_on_cleanup.
    // `state_ptr` is a pointer created via WasmCell::into_raw, and can therefore be passed as the userdata for `cleanup`.
    // ic0.call_perform is always safe to call.
    let err_code = unsafe {
        ic0::call_new(
            callee.as_ptr() as i32,
            callee.len() as i32,
            method.as_ptr() as i32,
            method.len() as i32,
            callback as usize as i32,
            state_ptr as i32,
            callback as usize as i32,
            state_ptr as i32,
        );

        ic0::call_data_append(args_raw.as_ptr() as i32, args_raw.len() as i32);
        payment_func();
        ic0::call_on_cleanup(cleanup as usize as i32, state_ptr as i32);
        ic0::call_perform()
    };

    // 0 is a special error code meaning call_simple call succeeded.
    if err_code != 0 {
        let mut state = state.borrow_mut();
        state.result = Some(Err((
            RejectionCode::from(err_code),
            "Couldn't send message".to_string(),
        )));
    }
    CallFuture { state }
}

/// Performs an asynchronous call to another canister via ic0.
pub fn call<T: ArgumentEncoder, R: for<'a> ArgumentDecoder<'a>>(
    id: Principal,
    method: &str,
    args: T,
) -> impl Future<Output = CallResult<R>> {
    let args_raw = encode_args(args).expect("Failed to encode arguments.");
    let fut = call_raw(id, method, &args_raw, 0);
    async {
        let bytes = fut.await?;
        decode_args(&bytes).map_err(|err| trap(&format!("{:?}", err)))
    }
}

/// Performs an asynchronous call to another canister and pay cycles at the same time.
pub fn call_with_payment<T: ArgumentEncoder, R: for<'a> ArgumentDecoder<'a>>(
    id: Principal,
    method: &str,
    args: T,
    cycles: u64,
) -> impl Future<Output = CallResult<R>> {
    let args_raw = encode_args(args).expect("Failed to encode arguments.");
    let fut = call_raw(id, method, &args_raw, cycles);
    async {
        let bytes = fut.await?;
        decode_args(&bytes).map_err(|err| trap(&format!("{:?}", err)))
    }
}

/// Performs an asynchronous call to another canister and pay cycles at the same time.
pub fn call_with_payment128<T: ArgumentEncoder, R: for<'a> ArgumentDecoder<'a>>(
    id: Principal,
    method: &str,
    args: T,
    cycles: u128,
) -> impl Future<Output = CallResult<R>> {
    let args_raw = encode_args(args).expect("Failed to encode arguments.");
    let fut = call_raw128(id, method, &args_raw, cycles);
    async {
        let bytes = fut.await?;
        decode_args(&bytes).map_err(|err| trap(&format!("{:?}", err)))
    }
}

/// Returns a result that maps over the call
///
/// It will be Ok(T) if the call succeeded (with T being the arg_data),
/// and [reject_message()] if it failed.
pub fn result<T: for<'a> ArgumentDecoder<'a>>() -> Result<T, String> {
    match reject_code() {
        RejectionCode::NoError => {
            decode_args(&arg_data_raw()).map_err(|e| format!("Failed to decode arguments: {}", e))
        }
        _ => Err(reject_message()),
    }
}

/// Returns the rejection code for the call.
pub fn reject_code() -> RejectionCode {
    // SAFETY: ic0.msg_reject_code is always safe to call.
    let code = unsafe { ic0::msg_reject_code() };
    RejectionCode::from(code)
}

/// Returns the rejection message.
pub fn reject_message() -> String {
    // SAFETY: ic0.msg_reject_msg_size is always safe to call.
    let len: u32 = unsafe { ic0::msg_reject_msg_size() as u32 };
    let mut bytes = vec![0; len as usize];
    // SAFETY: `bytes`, being mutable and allocated to `len` bytes, is safe to pass to ic0.msg_reject_msg_copy with no offset
    unsafe {
        ic0::msg_reject_msg_copy(bytes.as_mut_ptr() as i32, 0, len as i32);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Rejects the current call with the message.
pub fn reject(message: &str) {
    let err_message = message.as_bytes();
    // SAFETY: `err_message`, being &[u8], is a readable sequence of bytes, and therefore valid to pass to ic0.msg_reject.
    unsafe {
        ic0::msg_reject(err_message.as_ptr() as i32, err_message.len() as i32);
    }
}

/// An io::Write for message replies.
pub struct CallReplyWriter;

impl std::io::Write for CallReplyWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // SAFETY: buf, being &[u8], is a readable sequence of bytes, and therefore valid to pass to ic0.msg_reply_data_append.
        unsafe {
            ic0::msg_reply_data_append(buf.as_ptr() as i32, buf.len() as i32);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Replies to the current call with a candid argument.
pub fn reply<T: ArgumentEncoder>(reply: T) {
    write_args(&mut CallReplyWriter, reply).expect("Could not encode reply.");
    // SAFETY: ic0.msg_reply is always safe to call.
    unsafe {
        ic0::msg_reply();
    }
}

/// Returns the amount of cycles that were transferred by the caller
/// of the current call, and is still available in this message.
pub fn msg_cycles_available() -> u64 {
    // SAFETY: ic0.msg_cycles_available is always safe to call.
    unsafe { ic0::msg_cycles_available() as u64 }
}

/// Returns the amount of cycles that were transferred by the caller
/// of the current call, and is still available in this message.
pub fn msg_cycles_available128() -> u128 {
    let mut recv = 0u128;
    // SAFETY: recv is writable and sixteen bytes wide, and therefore is safe to pass to ic0.msg_cycles_available
    unsafe {
        ic0::msg_cycles_available128(&mut recv as *mut u128 as i32);
    }
    recv
}

/// Returns the amount of cycles that came back with the response as a refund.
///
/// The refund has already been added to the canister balance automatically.
pub fn msg_cycles_refunded() -> u64 {
    // SAFETY: ic0.msg_cycles_refunded is always safe to call
    unsafe { ic0::msg_cycles_refunded() as u64 }
}

/// Returns the amount of cycles that came back with the response as a refund.
///
/// The refund has already been added to the canister balance automatically.
pub fn msg_cycles_refunded128() -> u128 {
    let mut recv = 0u128;
    // SAFETY: recv is writable and sixteen bytes wide, and therefore is safe to pass to ic0.msg_cycles_refunded128
    unsafe {
        ic0::msg_cycles_refunded128(&mut recv as *mut u128 as i32);
    }
    recv
}

/// Moves cycles from the call to the canister balance.
///
/// The actual amount moved will be returned.
pub fn msg_cycles_accept(max_amount: u64) -> u64 {
    // SAFETY: ic0.msg_cycles_accept is always safe to call.
    unsafe { ic0::msg_cycles_accept(max_amount as i64) as u64 }
}

/// Moves cycles from the call to the canister balance.
///
/// The actual amount moved will be returned.
pub fn msg_cycles_accept128(max_amount: u128) -> u128 {
    let high = (max_amount >> 64) as u64;
    let low = (max_amount & u64::MAX as u128) as u64;
    let mut recv = 0u128;
    // SAFETY: `recv` is writable and sixteen bytes wide, and therefore safe to pass to ic0.msg_cycles_accept128
    unsafe {
        ic0::msg_cycles_accept128(high as i64, low as i64, &mut recv as *mut u128 as i32);
    }
    recv
}

/// Returns the argument data as bytes.
pub(crate) fn arg_data_raw() -> Vec<u8> {
    // SAFETY: ic0.msg_arg_data_size is always safe to call.
    let len: usize = unsafe { ic0::msg_arg_data_size() as usize };
    let mut bytes = vec![0u8; len as usize];
    // SAFETY: ic0.msg_arg_data_copy is safe to call if `bytes` is allocated to a capacity of `len`.
    unsafe {
        ic0::msg_arg_data_copy(bytes.as_mut_ptr() as i32, 0, len as i32);
    }
    bytes
}

/// Returns the argument data in the current call.
pub fn arg_data<R: for<'a> ArgumentDecoder<'a>>() -> R {
    let bytes = arg_data_raw();

    match decode_args(&bytes) {
        Err(e) => trap(&format!("{:?}", e)),
        Ok(r) => r,
    }
}

/// Accepts the ingress message.
pub fn accept_message() {
    // SAFETY: ic0.accept_message is always safe to call.
    unsafe {
        ic0::accept_message();
    }
}

/// Returns the name of current canister method.
pub fn method_name() -> String {
    // SAFETY: ic0.msg_method_name_size is always safe to call.
    let len: u32 = unsafe { ic0::msg_method_name_size() as u32 };
    let mut bytes = vec![0; len as usize];
    // SAFETY: `bytes` is writable and allocated to `len` bytes, and therefore can be safely passed to ic0.msg_method_name_copy
    unsafe {
        ic0::msg_method_name_copy(bytes.as_mut_ptr() as i32, 0, len as i32);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Pretends to have the Candid type `T`, but unconditionally errors
/// when serialized.
///
/// Usable, but not required, as metadata when using `#[query(reply = false)]`,
/// so an accurate Candid file can still be generated.
#[derive(Debug, Copy, Clone, Default)]
pub struct ManualReply<T: ?Sized>(PhantomData<T>);

impl<T: ?Sized> ManualReply<T> {
    /// Constructs a new `ManualReply`.
    #[allow(clippy::self_named_constructors)]
    pub const fn empty() -> Self {
        Self(PhantomData)
    }
    /// Replies with the given value and returns a new `ManualReply`,
    /// for a useful reply-then-return shortcut.
    pub fn all<U>(value: U) -> Self
    where
        U: ArgumentEncoder,
    {
        reply(value);
        Self::empty()
    }
    /// Replies with a one-element tuple around the given value and returns
    /// a new `ManualReply`, for a useful reply-then-return shortcut.
    pub fn one<U>(value: U) -> Self
    where
        U: CandidType,
    {
        reply((value,));
        Self::empty()
    }
}

impl<T> CandidType for ManualReply<T>
where
    T: CandidType + ?Sized,
{
    fn _ty() -> candid::types::Type {
        T::_ty()
    }
    /// Unconditionally errors.
    fn idl_serialize<S>(&self, _: S) -> Result<(), S::Error>
    where
        S: candid::types::Serializer,
    {
        Err(S::Error::custom("`Empty` cannot be serialized"))
    }
}

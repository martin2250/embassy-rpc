#![no_std]
#![doc = include_str!("../Readme.md")]

use core::cell::RefCell;
use core::future::poll_fn;
use core::ops::{Deref, DerefMut};
use core::task::Waker;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::RawMutex;

/// Error returned to the client when the server drops [`ServedRequest`] without calling
/// [`ServedRequest::respond`].
///
/// This is not a transport error: it means the server-side handler gave up without sending a
/// successful response (for example by returning early or unwinding after `serve`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestDroppedError;

/// In-memory request/response synchronization for async tasks.
///
/// This type is **not** a wire protocol. It connects a client async task that calls
/// [`RpcService::request`] with a server task that calls [`RpcService::serve`], using a
/// [`embassy_sync::blocking_mutex::Mutex`] and async wakers. It is suitable for `no_std` use when
/// paired with a mutex implementation appropriate for your platform ([`RawMutex`]).
///
/// # Concurrency
///
/// - **Single in-flight RPC:** Internal state holds at most one queued request and one response
///   slot. Design for one active request/response pair at a time.
/// - **Multiple clients:** Several tasks may call [`request`](Self::request); additional callers
///   wait until the current RPC completes (including delivery of [`RequestDroppedError`]).
///
/// # Type parameters
///
/// - **`M`:** Mutex raw type ([`RawMutex`]), e.g. `CriticalSectionRawMutex` on many MCUs.
/// - **`Req` / `Resp`:** Your message types. `Req` may borrow from the client for the duration of
///   the call; then the [`RpcService`] must not outlive those borrows (often modeled with a
///   stack-scoped service—see the crate README).
///
/// # Panics
///
/// [`ServedRequest::into_inner`] and [`ServedRequest`]'s [`Deref`] / [`DerefMut`] implementations
/// panic if the inner request was already taken.
pub struct RpcService<M, Req, Resp>
where
    M: RawMutex,
{
    state: Mutex<M, RefCell<State<Req, Resp>>>,
}

struct State<Req, Resp> {
    client_busy: bool,
    waiting_client_slot_waker: Option<Waker>,
    waiting_client_response_waker: Option<Waker>,
    waiting_server_waker: Option<Waker>,
    queued_request: Option<Req>,
    queued_response: Option<Result<Resp, RequestDroppedError>>,
}

impl<Req, Resp> State<Req, Resp> {
    const fn new() -> Self {
        Self {
            client_busy: false,
            waiting_client_slot_waker: None,
            waiting_client_response_waker: None,
            waiting_server_waker: None,
            queued_request: None,
            queued_response: None,
        }
    }
}

impl<M, Req, Resp> RpcService<M, Req, Resp>
where
    M: RawMutex,
{
    /// Creates an empty service. Safe to call in `const` contexts (e.g. `static` initializer).
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(RefCell::new(State::new())),
        }
    }

    /// Sends `req` and waits until the server responds or drops the [`ServedRequest`].
    ///
    /// If another client is already in an RPC, this call waits until that RPC fully completes
    /// (including waking the next waiter for the client slot) before sending `req`.
    ///
    /// # Errors
    ///
    /// Returns [`Err(RequestDroppedError)`](RequestDroppedError) if the server drops
    /// [`ServedRequest`] without calling [`ServedRequest::respond`].
    pub async fn request(&self, req: Req) -> Result<Resp, RequestDroppedError> {
        self.acquire_client_slot().await;

        self.state.lock(|state| {
            let mut state = state.borrow_mut();
            state.queued_request = Some(req);
            if let Some(waker) = state.waiting_server_waker.take() {
                waker.wake();
            }
        });

        let result = poll_fn(|cx| {
            self.state.lock(|state| {
                let mut state = state.borrow_mut();
                if let Some(resp) = state.queued_response.take() {
                    state.client_busy = false;
                    if let Some(waker) = state.waiting_client_slot_waker.take() {
                        waker.wake();
                    }
                    return core::task::Poll::Ready(resp);
                }

                state.waiting_client_response_waker = Some(cx.waker().clone());
                core::task::Poll::Pending
            })
        })
        .await;

        result
    }

    /// Waits until a client submits a request via [`Self::request`], then returns it wrapped in
    /// [`ServedRequest`].
    ///
    /// The server must eventually call [`ServedRequest::respond`] or drop the handle; dropping
    /// notifies the client with [`RequestDroppedError`].
    pub async fn serve(&self) -> ServedRequest<'_, M, Req, Resp> {
        let req = poll_fn(|cx| {
            self.state.lock(|state| {
                let mut state = state.borrow_mut();
                if let Some(req) = state.queued_request.take() {
                    return core::task::Poll::Ready(req);
                }

                state.waiting_server_waker = Some(cx.waker().clone());
                core::task::Poll::Pending
            })
        })
        .await;

        ServedRequest {
            req: Some(req),
            state: &self.state,
            completed: false,
        }
    }

    async fn acquire_client_slot(&self) {
        poll_fn(|cx| {
            self.state.lock(|state| {
                let mut state = state.borrow_mut();
                if !state.client_busy {
                    state.client_busy = true;
                    return core::task::Poll::Ready(());
                }

                state.waiting_client_slot_waker = Some(cx.waker().clone());
                core::task::Poll::Pending
            })
        })
        .await;
    }
}

/// Server-side handle for one request taken from [`RpcService::serve`].
///
/// Dereferences to the inner `Req` via [`Deref`] and [`DerefMut`] for ergonomic access.
///
/// # Completion
///
/// - Call [`Self::respond`] with a successful `Resp` to complete the RPC.
/// - Drop this value without calling [`Self::respond`] to complete the RPC with
///   [`RequestDroppedError`] on the client.
///
/// # Panics
///
/// [`Deref::deref`], [`DerefMut::deref_mut`], and [`Self::into_inner`] panic if the inner request
/// was already consumed (for example after [`Self::respond`]).
pub struct ServedRequest<'a, M, Req, Resp>
where
    M: RawMutex,
{
    req: Option<Req>,
    state: &'a Mutex<M, RefCell<State<Req, Resp>>>,
    completed: bool,
}

impl<'a, M, Req, Resp> ServedRequest<'a, M, Req, Resp>
where
    M: RawMutex,
{
    /// Completes the RPC with `resp` and consumes `self`.
    ///
    /// The waiting client receives `Ok(resp)`.
    pub fn respond(mut self, resp: Resp) {
        self.state.lock(|state| {
            let mut state = state.borrow_mut();
            state.queued_response = Some(Ok(resp));
            if let Some(waker) = state.waiting_client_response_waker.take() {
                waker.wake();
            }
        });
        let _ = self.req.take();
        self.completed = true;
    }

    /// Extracts the inner request value, consuming `self`.
    ///
    /// # Panics
    ///
    /// Panics if the inner value was already taken (for example after [`Self::respond`]).
    ///
    /// # Effect on the client
    ///
    /// This does not send a successful response. When `self` is dropped after this call, the
    /// waiting client receives [`Err(RequestDroppedError)`](RequestDroppedError) (same as dropping
    /// [`ServedRequest`] without calling [`Self::respond`]).
    pub fn into_inner(mut self) -> Req {
        self.req
            .take()
            .expect("ServedRequest inner request already taken")
    }
}

impl<'a, M, Req, Resp> Deref for ServedRequest<'a, M, Req, Resp>
where
    M: RawMutex,
{
    type Target = Req;

    fn deref(&self) -> &Self::Target {
        self.req
            .as_ref()
            .expect("ServedRequest inner request already taken")
    }
}

impl<'a, M, Req, Resp> DerefMut for ServedRequest<'a, M, Req, Resp>
where
    M: RawMutex,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.req
            .as_mut()
            .expect("ServedRequest inner request already taken")
    }
}

impl<'a, M, Req, Resp> Drop for ServedRequest<'a, M, Req, Resp>
where
    M: RawMutex,
{
    fn drop(&mut self) {
        if !self.completed {
            self.state.lock(|state| {
                let mut state = state.borrow_mut();
                state.queued_response = Some(Err(RequestDroppedError));
                if let Some(waker) = state.waiting_client_response_waker.take() {
                    waker.wake();
                }
            });
        }
    }
}

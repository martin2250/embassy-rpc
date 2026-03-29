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
/// - **Client cancellation:** If the async task awaiting [`Self::request`] is dropped (executor
///   cancellation), the service releases the client slot once the in-flight RPC is fully finished
///   (including after the server responds or drops [`ServedRequest`]). Dropping the client future
///   after the request was queued but before the server takes it removes the queued request and
///   frees the slot immediately.
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
    /// Client [`RpcService::request`] future was dropped while waiting; server must finish without
    /// delivering a response to that client.
    client_abandoned: bool,
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
            client_abandoned: false,
            waiting_client_slot_waker: None,
            waiting_client_response_waker: None,
            waiting_server_waker: None,
            queued_request: None,
            queued_response: None,
        }
    }
}

/// When dropped without [`Self::defuse`], cleans up after a cancelled [`RpcService::request`].
struct InFlightGuard<'a, M, Req, Resp>
where
    M: RawMutex,
{
    service: &'a RpcService<M, Req, Resp>,
    disarm: bool,
}

impl<'a, M, Req, Resp> InFlightGuard<'a, M, Req, Resp>
where
    M: RawMutex,
{
    fn new(service: &'a RpcService<M, Req, Resp>) -> Self {
        Self {
            service,
            disarm: false,
        }
    }

    fn defuse(mut self) {
        self.disarm = true;
        core::mem::forget(self);
    }
}

impl<'a, M, Req, Resp> Drop for InFlightGuard<'a, M, Req, Resp>
where
    M: RawMutex,
{
    fn drop(&mut self) {
        if self.disarm {
            return;
        }

        self.service.state.lock(|state| {
            let mut s = state.borrow_mut();
            if let Some(req) = s.queued_request.take() {
                drop(req);
                s.client_abandoned = false;
                s.client_busy = false;
                if let Some(w) = s.waiting_client_slot_waker.take() {
                    w.wake();
                }
                return;
            }
            if let Some(resp) = s.queued_response.take() {
                drop(resp);
                s.client_abandoned = false;
                s.client_busy = false;
                if let Some(w) = s.waiting_client_slot_waker.take() {
                    w.wake();
                }
                return;
            }
            s.client_abandoned = true;
        });
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
    ///
    /// # Cancellation
    ///
    /// If this future is dropped while waiting for a response, the queued request is removed if
    /// the server has not taken it yet; otherwise the server continues with [`ServedRequest`] and
    /// the response is discarded when the server completes. The client slot becomes available again
    /// after that completion (or immediately when the queued request is dropped).
    pub async fn request(&self, req: Req) -> Result<Resp, RequestDroppedError> {
        self.acquire_client_slot().await;

        self.state.lock(|state| {
            let mut state = state.borrow_mut();
            state.queued_request = Some(req);
            if let Some(waker) = state.waiting_server_waker.take() {
                waker.wake();
            }
        });

        let in_flight = InFlightGuard::new(self);

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

        in_flight.defuse();
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

impl<M, Req, Resp> Default for RpcService<M, Req, Resp>
where
    M: RawMutex,
{
    fn default() -> Self {
        Self::new()
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
            if state.client_abandoned {
                state.client_abandoned = false;
                state.client_busy = false;
                if let Some(waker) = state.waiting_client_slot_waker.take() {
                    waker.wake();
                }
            } else {
                state.queued_response = Some(Ok(resp));
                if let Some(waker) = state.waiting_client_response_waker.take() {
                    waker.wake();
                }
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
                if state.client_abandoned {
                    state.client_abandoned = false;
                    state.client_busy = false;
                    if let Some(waker) = state.waiting_client_slot_waker.take() {
                        waker.wake();
                    }
                } else {
                    state.queued_response = Some(Err(RequestDroppedError));
                    if let Some(waker) = state.waiting_client_response_waker.take() {
                        waker.wake();
                    }
                }
            });
        }
    }
}

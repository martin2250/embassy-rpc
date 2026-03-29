#![no_std]

use core::cell::RefCell;
use core::future::poll_fn;
use core::ops::{Deref, DerefMut};
use core::task::Waker;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::RawMutex;

/// Error returned to the client when a served request is dropped
/// without producing a response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestDroppedError;

/// Minimal RPC-like synchronization primitive for request/response.
///
/// This POC supports one in-flight request at a time. Multiple clients can
/// call `request()`, but they are serialized internally.
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
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(RefCell::new(State::new())),
        }
    }

    /// Send a request and wait for either response or drop-notification.
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

    /// Wait for the next request from clients.
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

/// Server-side request wrapper.
///
/// Deref/DerefMut are implemented to make working with the inner request ergonomic.
/// If dropped without calling `respond`, the client gets `RequestDroppedError`.
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

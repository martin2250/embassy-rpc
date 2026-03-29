//! Integration tests for `embassy-rpc`.
//!
//! `ThreadModeRawMutex` (with `embassy-sync`’s `std` feature) only allows lock/drop on a thread
//! whose name is `"main"`. The default Rust test harness runs each test on a worker thread, so
//! we run the async body on a child thread explicitly named `"main"`.

use embassy_rpc::{RequestDroppedError, RpcService};
use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use std::future::Future;
use std::sync::Arc;
use tokio::join;
use tokio::runtime::Builder;
use tokio::time::{Duration, timeout};

fn run_on_main_named_thread<F>(f: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    std::thread::Builder::new()
        .name("main".to_string())
        .spawn(move || {
            let rt = Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(f);
        })
        .expect("spawn")
        .join()
        .expect("thread join");
}

/// Like the README sketch: the client lends a mutable buffer to the server for the duration of the RPC.
struct BufferRequest<'a> {
    buffer: &'a mut [u8],
}

#[test]
fn server_writes_through_client_buffer_slice() {
    run_on_main_named_thread(async {
        // `BufferRequest` carries `&mut [u8]`; keep the service on the stack (no `Arc`) so `Req`
        // is not forced to unify with a longer-lived type parameter from `Arc`.
        let mut buf = [0u8; 8];

        // `RpcService<BufferRequest<'a>>` ties `'a` to the service value; drop `service` before
        // reading `buf` again after the RPC (the compiler keeps `'a` live for `service`'s scope).
        let client_result = {
            let service = RpcService::<ThreadModeRawMutex, BufferRequest<'_>, ()>::new();

            let server = async {
                let mut req = service.serve().await;
                req.buffer[0] = 0xab;
                req.buffer[1] = 0xcd;
                req.buffer[2..].fill(0x7e);
                req.respond(());
            };

            let client = async {
                service
                    .request(BufferRequest { buffer: &mut buf })
                    .await
            };

            let (_, r) = join!(server, client);
            r
        };

        assert_eq!(buf, [0xab, 0xcd, 0x7e, 0x7e, 0x7e, 0x7e, 0x7e, 0x7e]);
        assert_eq!(client_result, Ok(()));
    });
}

#[test]
fn round_trip_response_is_delivered() {
    run_on_main_named_thread(async {
        let service = Arc::new(RpcService::<ThreadModeRawMutex, u32, u32>::new());

        let server = {
            let service = Arc::clone(&service);
            async move {
                let req = service.serve().await;
                req.respond(10);
            }
        };

        let client = {
            let service = Arc::clone(&service);
            async move { service.request(5).await }
        };

        let (_, client_result) = join!(server, client);
        assert_eq!(client_result, Ok(10));
    });
}

#[test]
fn dropped_request_returns_error() {
    run_on_main_named_thread(async {
        let service = Arc::new(RpcService::<ThreadModeRawMutex, u32, u32>::new());

        let server = {
            let service = Arc::clone(&service);
            async move {
                let _req = service.serve().await;
                // Dropped intentionally without respond().
            }
        };

        let client = {
            let service = Arc::clone(&service);
            async move { service.request(42).await }
        };

        let (_, client_result) = join!(server, client);
        assert_eq!(client_result, Err(RequestDroppedError));
    });
}

#[test]
fn server_can_wait_before_client_requests() {
    run_on_main_named_thread(async {
        let service = Arc::new(RpcService::<ThreadModeRawMutex, u32, u32>::new());

        let server = {
            let service = Arc::clone(&service);
            async move {
                let req = service.serve().await;
                req.respond(123);
            }
        };

        let client = {
            let service = Arc::clone(&service);
            async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                service.request(1).await
            }
        };

        let result = timeout(Duration::from_secs(1), async {
            let (_, client_result) = join!(server, client);
            client_result
        })
        .await
        .expect("operation timed out");

        assert_eq!(result, Ok(123));
    });
}

#[test]
fn concurrent_clients_are_serialized_and_complete() {
    run_on_main_named_thread(async {
        let service = Arc::new(RpcService::<ThreadModeRawMutex, u32, u32>::new());

        let server = {
            let service = Arc::clone(&service);
            async move {
                for _ in 0..2 {
                    let req = service.serve().await;
                    let value = *req;
                    req.respond(value + 1);
                }
            }
        };

        let c1 = {
            let service = Arc::clone(&service);
            async move { service.request(10).await }
        };

        let c2 = {
            let service = Arc::clone(&service);
            async move { service.request(20).await }
        };

        let (r1, r2) = timeout(Duration::from_secs(1), async {
            let (_, a, b) = join!(server, c1, c2);
            (a, b)
        })
        .await
        .expect("operation timed out");

        assert_eq!(r1, Ok(11));
        assert_eq!(r2, Ok(21));
    });
}

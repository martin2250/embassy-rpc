//! Additional integration tests focused on uncovered control-flow paths.

use embassy_rpc::{RequestDroppedError, RpcService};
use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use std::future::Future;
use std::sync::Arc;
use tokio::join;
use tokio::runtime::Builder;
use tokio::sync::oneshot;
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

#[test]
fn into_inner_consumes_request_and_client_receives_dropped_error() {
    run_on_main_named_thread(async {
        let service = Arc::new(RpcService::<ThreadModeRawMutex, u32, u32>::new());

        let server = {
            let service = Arc::clone(&service);
            async move {
                let req = service.serve().await;
                let inner = req.into_inner();
                assert_eq!(inner, 42);
                // Drop after `into_inner` without `respond`.
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
fn cancelled_client_before_server_takes_request_wakes_waiting_slot_client() {
    run_on_main_named_thread(async {
        let service = Arc::new(RpcService::<ThreadModeRawMutex, u32, u32>::new());

        let client_cancelled = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.request(1).await })
        };

        let client_waiting = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.request(2).await })
        };

        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        client_cancelled.abort();
        let _ = client_cancelled.await;

        let server = {
            let service = Arc::clone(&service);
            tokio::spawn(async move {
                let req = service.serve().await;
                let n = *req;
                req.respond(n + 10);
            })
        };

        let client_waiting_result = timeout(Duration::from_secs(1), client_waiting)
            .await
            .expect("client_waiting timed out")
            .expect("client_waiting task panicked");
        assert_eq!(client_waiting_result, Ok(12));

        timeout(Duration::from_secs(1), server)
            .await
            .expect("server timed out")
            .expect("server task panicked");
    });
}

#[test]
fn cancelled_client_after_server_takes_request_and_responds_wakes_waiting_slot_client() {
    run_on_main_named_thread(async {
        let service = Arc::new(RpcService::<ThreadModeRawMutex, u32, u32>::new());
        let (taken_tx, taken_rx) = oneshot::channel();
        let (continue_tx, continue_rx) = oneshot::channel();

        let server = {
            let service = Arc::clone(&service);
            tokio::spawn(async move {
                let req = service.serve().await;
                taken_tx.send(()).expect("signal that request was taken");

                continue_rx.await.expect("continue signal");
                req.respond(99);

                let req2 = service.serve().await;
                req2.respond(7);
            })
        };

        let client_cancelled = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.request(1).await })
        };

        timeout(Duration::from_secs(1), taken_rx)
            .await
            .expect("wait for first request timed out")
            .expect("taken signal channel closed");

        let client_waiting = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.request(2).await })
        };

        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        client_cancelled.abort();
        let _ = client_cancelled.await;

        continue_tx
            .send(())
            .expect("send server continue signal");

        let client_waiting_result = timeout(Duration::from_secs(1), client_waiting)
            .await
            .expect("client_waiting timed out")
            .expect("client_waiting task panicked");
        assert_eq!(client_waiting_result, Ok(7));

        timeout(Duration::from_secs(1), server)
            .await
            .expect("server timed out")
            .expect("server task panicked");
    });
}

#[test]
fn cancelled_client_after_server_takes_request_and_drops_wakes_waiting_slot_client() {
    run_on_main_named_thread(async {
        let service = Arc::new(RpcService::<ThreadModeRawMutex, u32, u32>::new());
        let (taken_tx, taken_rx) = oneshot::channel();
        let (continue_tx, continue_rx) = oneshot::channel();

        let server = {
            let service = Arc::clone(&service);
            tokio::spawn(async move {
                {
                    let _req = service.serve().await;
                    taken_tx.send(()).expect("signal that request was taken");
                    continue_rx.await.expect("continue signal");
                    // Drop without respond after client was cancelled.
                }

                let req2 = service.serve().await;
                req2.respond(42);
            })
        };

        let client_cancelled = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.request(1).await })
        };

        timeout(Duration::from_secs(1), taken_rx)
            .await
            .expect("wait for first request timed out")
            .expect("taken signal channel closed");

        let client_waiting = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.request(2).await })
        };

        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        client_cancelled.abort();
        let _ = client_cancelled.await;

        continue_tx
            .send(())
            .expect("send server continue signal");

        let client_waiting_result = timeout(Duration::from_secs(1), client_waiting)
            .await
            .expect("client_waiting timed out")
            .expect("client_waiting task panicked");
        assert_eq!(client_waiting_result, Ok(42));

        timeout(Duration::from_secs(1), server)
            .await
            .expect("server timed out")
            .expect("server task panicked");
    });
}

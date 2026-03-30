# embassy-rpc

`no_std` request/response synchronization for async tasks (Embassy-style executors). This is **not** a wire protocol: it is an in-memory channel built on [`embassy_sync`](https://docs.rs/embassy-sync) mutexes and async wakers.

## What this crate does

- **Single in-flight RPC:** At most one request and one response are queued in the internal state machine.
- **Client:** `RpcService::request` sends a request and awaits `Result<Resp, RequestDroppedError>`.
- **Server:** `RpcService::serve` waits for the next request and returns a `ServedRequest`. The server must call `ServedRequest::respond` with a success value, or drop the handle to signal failure to the client.
- **Multiple clients:** Several tasks may call `request()`, but they are **serialized**: a second caller blocks until the previous RPC fully completes (response delivered and the client slot released).

## Usage

1. Create a shared [`RpcService<M, Req, Resp>`](crate::RpcService) with a mutex type `M: RawMutex` appropriate for your platform (e.g. `CriticalSectionRawMutex` on many MCUs).
2. Run a server task that loops on `serve().await`, inspects or mutates the request (often via `Deref` / `DerefMut` on [`ServedRequest`](crate::ServedRequest)), then calls `respond(resp)`.
3. Run client task(s) that call `request(req).await` and handle `Ok(resp)` vs `Err(RequestDroppedError)`.

`Req` and `Resp` are yours: they can be integers, enums, or types that borrow data from the client for the duration of the call (see the example below).

## Example (simple types)

```rust,ignore
use embassy_rpc::RpcService;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

static SERVICE: RpcService<CriticalSectionRawMutex, u32, u32> = RpcService::new();

async fn server() {
    loop {
        let req = SERVICE.serve().await;
        let value = *req;
        req.respond(value.saturating_add(1));
    }
}

async fn client() {
    match SERVICE.request(5).await {
        Ok(n) => assert_eq!(n, 6),
        Err(embassy_rpc::RequestDroppedError) => { /* server dropped the request */ }
    }
}
```

## Example (client lends a buffer)

`Req` can carry `&mut [u8]` (or other borrows) so the server writes into the client’s memory for the duration of one RPC. The `RpcService` must live **no longer** than the borrowed data, so keep it on the stack (or in an owning struct) together with the buffer—see the `server_writes_through_client_buffer_slice` integration test in this repository.

```rust,ignore
use embassy_rpc::RpcService;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

struct BufferRequest<'a> {
    buffer: &'a mut [u8],
}

async fn server_task(service: &RpcService<CriticalSectionRawMutex, BufferRequest<'_>, ()>) {
    let mut req = service.serve().await;
    req.buffer[0] = 0xab;
    req.respond(());
}

async fn client_task(
    service: &RpcService<CriticalSectionRawMutex, BufferRequest<'_>, ()>,
    buf: &mut [u8],
) -> Result<(), embassy_rpc::RequestDroppedError> {
    service.request(BufferRequest { buffer: buf }).await
}
```

Run `server_task` and `client_task` **concurrently** on the same `service` (spawn or join, depending on your executor).

## Considerations

- **`RequestDroppedError`:** Treat `Err(RequestDroppedError)` as “no normal response” (server dropped `ServedRequest` without calling `respond`). Do not assume `Ok` if the server may abort handling.
- **`ServedRequest` misuse:** `into_inner` and `Deref`/`DerefMut` panic if the inner value was already taken. After `respond`, the value is consumed.
- **Lifetimes:** If `Req` borrows from the client (e.g. `&mut [u8]`), the `RpcService` value must not outlive those borrows. A stack-scoped service is often the right shape; `Arc<RpcService<...>>` with stack-borrowed payloads is usually incompatible without restructuring.
- **No built-in timeouts:** Compose with your executor’s timeout helpers if you need deadlines on `request()` or `serve()`.
- **Integration tests and `ThreadModeRawMutex`:** With `embassy-sync`’s `std` feature, `ThreadModeRawMutex` may only be used from a thread named `"main"`. The crate’s tests spawn such a thread; on embedded firmware, prefer a mutex that matches your interrupt/preemption model.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.



```rust

struct Request<'a> {
    buffer: &mut [u8],
};
struct Response;

static SERVICE: RpcService<CriticalSectionRawMutex, Request, Response> = RpcService::new();

async fn server() {
    loop {
        let mut req = SERVICE.serve().await;
        // Request can contain mutable references that are valid for the lifetime of Request

        // process request
        req.buffer[0] = 1;

        if (error) {
            // req is dropped, client receives RequestDroppedError
        } else {
            // this consumes req
            req.respond(Response);
        }
    }
}

async fn client() {
    let mut buffer = [0; 64];

    match SERVICE.request(Request{ buffer: &mut buffer }).await {
        Ok(resp: Response) => (),
        Err(RequestDroppedError) => (),
    }
}

```
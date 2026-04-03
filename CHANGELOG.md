# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **Crate rename:** The package is now named `embedded-rpc` (Rust identifier `embedded_rpc`). It was previously `embassy-rpc` / `embassy_rpc`. Update `Cargo.toml` (`embedded-rpc = "…"`) and imports (`use embedded_rpc::…`). Documentation is at [docs.rs/embedded-rpc](https://docs.rs/embedded-rpc). The Git repository is now [github.com/martin2250/embedded-rpc](https://github.com/martin2250/embedded-rpc) (rename the remote URL if you still have the old clone path).

## [0.2.0] - 2026-04-01

### Changed

- **`RpcService::serve`** now returns `(Req, ServedRequest<'_, M, Req, Resp>)` instead of `ServedRequest` alone. The request value is the first element; the second is the completion handle for `ServedRequest::respond` and drop behavior.

### Removed

- **`ServedRequest`** no longer implements `Deref` or `DerefMut`; use the `Req` value returned from `serve` directly.
- **`ServedRequest::into_inner`**, which is redundant now that the request is not stored inside `ServedRequest`.

// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud-tape — typed REST client for the Daytona API
//
// Wraps only the endpoints baud calls:
//   create/start/stop/archive/delete sandbox, exec, file upload/download,
//   preview URL
//
// Hidden behind the Backend trait shared with baud-tape-local.
// Nothing above the trait may import this crate.
//
// Retries with backoff; recorded-fixture contract tests.

pub mod backend;
pub mod daytona;
pub mod types;

pub use backend::Backend;
pub use types::{SandboxStatus, SandboxSpec, ExecResult, TapeState};

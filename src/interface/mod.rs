//! Interface layer: the command line and the webhook server.
//!
//! `cli` is also the composition root of the `ano` binary: it builds the
//! infrastructure adapters and hands them to the application layer.

pub mod cli;
pub mod webhook;

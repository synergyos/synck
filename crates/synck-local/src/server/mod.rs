//! Reference Synck sync server: protocol 1 over existing repository
//! directories, for one owner's devices, agents and the people they give a
//! token. No users, folder permissions, encryption or mail; those are SynckHub.

mod error;
mod http;
mod jobs;
mod lock;
mod state;
pub mod tokens;
mod uploads;

pub use error::{Error, Result};
pub use http::{Server, init, router, serve};

//! Strongly-typed CDP protocol envelopes.
//!
//! Each submodule mirrors one CDP domain and exposes plain `Serialize`/`Deserialize`
//! structs plus free async helpers taking `&CdpClient`. See `transport` for the
//! untyped fallback (`send`).

pub mod accessibility;
pub mod browser;
pub mod dom;
pub mod emulation;
pub mod fetch;
pub mod input;
pub mod network;
pub mod page;
pub mod runtime;
pub mod target;

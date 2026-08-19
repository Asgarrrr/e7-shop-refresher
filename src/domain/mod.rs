//! Pure domain: the shop model, the client-side interest filter, and the
//! refresh-loop controller. No I/O, no transport — testable without the
//! game or the server.

pub mod control;
pub mod filter;
pub mod shop;

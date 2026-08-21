//! Security primitives: password hashing, secret encryption, rate limiting,
//! and SSRF-safe outbound HTTP.

pub mod outb {
    //! Re-export of the outbound/SSRF layer under a stable path.
    pub use super::ssrf::*;
}

pub mod password;
pub mod ratelimit;
pub mod secrets;
pub mod ssrf;
pub mod totp;

pub use ssrf::{validate_url, OutboundPolicy};

//! Network primitives that do not depend on the host: IP networks in CIDR notation and their
//! subtraction, which egress rules use.

pub mod cidr;

pub use cidr::Cidr;

//! The school side: everything the core knows about talking to a TronClass tenant.

pub mod answer;
pub mod course_context;
pub mod courses;
mod endpoints;
pub mod login;
pub mod quiz;
pub mod radar;
pub mod rollcall;
pub mod teacher_qr;

pub use endpoints::Endpoints;

//! Marshal's own pieces of myko#129's auth provider. The provider itself
//! (authenticator, token store, TLS) is installed by the gate stories
//! (MAR-23, MAR-24); the types here are what they install.

pub mod session_binding;

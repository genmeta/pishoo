//! DNS publishing (mDNS + H3 DNS) from the root process.
//!
//! Thin wrapper around [`gateway::dns`] for the root process context.
//! The root is responsible for publishing DNS records for all servers
//! (both root-local and worker-owned).

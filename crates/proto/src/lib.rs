//! Generated gRPC types for the control plane <-> node protocol.

pub mod node {
    pub mod v1 {
        tonic::include_proto!("modula.node.v1");

        /// Wire protocol version. Bump on incompatible changes; the server
        /// accepts this version and the one before it.
        pub const PROTOCOL: u32 = 1;
    }
}

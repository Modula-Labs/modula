//! Generated gRPC types for the control plane <-> node protocol.

pub mod node {
    pub mod v1 {
        tonic::include_proto!("modula.node.v1");

        /// Wire protocol version. Bump on incompatible changes; the server
        /// accepts this version and the one before it.
        pub const PROTOCOL: u32 = 1;

        /// Where a FUNCTION build leaves its bundle in the image it builds.
        pub const FUNCTION_OUTPUT_DIR: &str = "/fn";
        /// The bundle's one module, in [`FUNCTION_OUTPUT_DIR`].
        pub const FUNCTION_MODULE: &str = "worker.js";
    }
}

#[cfg(test)]
mod tests {
    use super::node::v1 as pb;
    use prost::Message;

    /// Edge rules, access logs and unsupported-module reports cross the
    /// node stream intact.
    #[test]
    fn edge_messages_round_trip() {
        let route = pb::Route {
            hostname: "api.example.com".into(),
            deployment_id: "d".into(),
            port: 8080,
            remote_upstreams: vec!["10.80.1.2:8080".into()],
            proxied: true,
            rules: vec![pb::EdgeRule {
                any: vec![pb::EdgeMatchGroup {
                    all: vec![pb::EdgeCondition {
                        negate: true,
                        kind: Some(pb::edge_condition::Kind::Country(pb::EdgeList {
                            values: vec!["RU".into()],
                        })),
                    }],
                }],
                action: Some(pb::edge_rule::Action::Headers(pb::EdgeHeaders {
                    set: [("X-A".to_owned(), "1".to_owned())].into(),
                    remove: vec!["Server".into()],
                })),
            }],
        };
        assert_eq!(
            pb::Route::decode(route.encode_to_vec().as_slice()).unwrap(),
            route
        );
        let m = pb::NodeMessage {
            msg: Some(pb::node_message::Msg::HttpLogBatch(pb::HttpLogBatch {
                requests: vec![pb::HttpRequestLog {
                    deployment_id: "d".into(),
                    at_unix_ms: 1,
                    host: "api.example.com".into(),
                    method: "GET".into(),
                    path: "/".into(),
                    status: 200,
                    duration_us: 3000,
                    bytes: 10,
                    client_ip: "198.51.100.7".into(),
                    upstream: "10.88.0.2:8080".into(),
                    user_agent: "curl/8".into(),
                }],
                dropped: 2,
            })),
        };
        assert_eq!(
            pb::NodeMessage::decode(m.encode_to_vec().as_slice()).unwrap(),
            m
        );
        let report = pb::StateReport {
            generation: 3,
            workloads: vec![],
            edge_unsupported: vec!["cache".into()],
        };
        assert_eq!(
            pb::StateReport::decode(report.encode_to_vec().as_slice()).unwrap(),
            report
        );
    }
}

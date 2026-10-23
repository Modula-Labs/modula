//! Folding what a build learned from the repository's config file
//! (`railway.toml`, `modula.toml`, …) into the deployment's config snapshot,
//! so a restart or rollback of that deployment runs the same way.

use modula_proto::node::v1 as pb;

use super::config::{RestartPolicy, ServiceConfig};

/// The snapshot with the build's runtime settings applied, if they are valid;
/// otherwise the snapshot unchanged (the node validated them, but the server
/// never trusts a node blindly).
pub fn apply(mut cfg: ServiceConfig, out: &pb::BuildOutput) -> ServiceConfig {
    let before = cfg.clone();
    if out.has_command {
        cfg.start_command = (!out.command.is_empty()).then(|| out.command.clone());
    }
    if !out.healthcheck_path.is_empty() {
        cfg.healthcheck_path = Some(out.healthcheck_path.clone());
    }
    if out.healthcheck_timeout_secs > 0 {
        cfg.healthcheck_timeout_secs = out.healthcheck_timeout_secs;
    }
    match pb::RestartPolicy::try_from(out.restart) {
        Ok(pb::RestartPolicy::OnFailure) => cfg.restart_policy = RestartPolicy::OnFailure,
        Ok(pb::RestartPolicy::Always) => cfg.restart_policy = RestartPolicy::Always,
        Ok(pb::RestartPolicy::Never) => cfg.restart_policy = RestartPolicy::Never,
        _ => {}
    }
    if cfg.validate().is_ok() { cfg } else { before }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> ServiceConfig {
        serde_json::from_value(serde_json::json!({
            "source": { "type": "repo", "repo": "acme/app", "branch": "main" }
        }))
        .unwrap()
    }

    #[test]
    fn applies_valid_and_rejects_invalid() {
        let out = pb::BuildOutput {
            has_command: true,
            command: vec!["/bin/sh".into(), "-c".into(), "node server.js".into()],
            healthcheck_path: "/health".into(),
            healthcheck_timeout_secs: 60,
            restart: pb::RestartPolicy::Always as i32,
            ..Default::default()
        };
        let c = apply(base(), &out);
        assert_eq!(c.start_command.as_deref().unwrap()[2], "node server.js");
        assert_eq!(c.healthcheck_path.as_deref(), Some("/health"));
        assert_eq!(
            (c.healthcheck_timeout_secs, c.restart_policy),
            (60, RestartPolicy::Always)
        );

        let bad = pb::BuildOutput {
            healthcheck_path: "no-slash".into(),
            ..out
        };
        assert_eq!(
            apply(base(), &bad),
            base(),
            "an invalid value leaves the snapshot alone"
        );
        assert_eq!(apply(base(), &pb::BuildOutput::default()), base());
    }
}

//! The installer and the release files it downloads, served by the instance
//! itself so a server joining it installs the same version. Both are public:
//! they hold no secrets, and the binaries are checked against signed checksums.

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::state::AppState;

const SCRIPT: &str = include_str!("../../../../deploy/install.sh");

/// Release files a joining server may fetch. Never a path: a fixed set of names.
const FILES: &[&str] = &[
    "SHA256SUMS",
    "SHA256SUMS.sig",
    "modula-node-linux-amd64",
    "modula-node-linux-arm64",
    "caddy-linux-amd64",
    "caddy-linux-arm64",
    "buildctl-linux-amd64",
    "buildctl-linux-arm64",
    "railpack-linux-amd64",
    "railpack-linux-arm64",
];

/// `install.sh` with this instance's version and release URL filled in.
pub fn script(cfg: &crate::config::Config) -> String {
    let releases = match cfg.release_dir {
        Some(_) => format!("{}/api/releases?file=", cfg.public_origin()),
        None => String::new(),
    };
    SCRIPT
        .replacen("__MODULA_VERSION__", env!("CARGO_PKG_VERSION"), 1)
        .replacen("__MODULA_SERVER_RELEASES__", &releases, 1)
}

pub async fn install_script(State(state): State<AppState>) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        script(&state.cfg),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct FileQuery {
    file: String,
}

pub async fn release_file(
    State(state): State<AppState>,
    Query(q): Query<FileQuery>,
) -> AppResult<Response> {
    let dir = state.cfg.release_dir.as_ref().ok_or(AppError::NotFound)?;
    let name = FILES
        .iter()
        .find(|f| **f == q.file)
        .ok_or(AppError::NotFound)?;
    let file = match tokio::fs::File::open(dir.join(name)).await {
        Ok(f) => f,
        // This instance only keeps its own architecture.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(AppError::NotFound),
        Err(e) => return Err(anyhow::Error::from(e).into()),
    };
    let len = file.metadata().await.map_err(anyhow::Error::from)?.len();
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (header::CONTENT_LENGTH, len.to_string()),
        ],
        Body::from_stream(tokio_util::io::ReaderStream::new(file)),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_filled() {
        let mut cfg = crate::config::Config::for_tests("https://deploy.example.com");
        let s = script(&cfg);
        assert!(!s.contains("__MODULA_VERSION__"));
        assert!(s.contains(&format!("MODULA_VERSION:-{}", env!("CARGO_PKG_VERSION"))));
        assert!(s.contains("SERVER_RELEASES=\"\""));
        cfg.release_dir = Some("/opt/modula/current".into());
        assert!(
            script(&cfg)
                .contains("SERVER_RELEASES=\"https://deploy.example.com/api/releases?file=\"")
        );
    }
}

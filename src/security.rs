use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use ipnet::IpNet;

use crate::config::RootConfig;
use crate::error::ApiError;

#[derive(Debug, Clone)]
pub struct IpAllowlist(Arc<Vec<IpNet>>);

impl IpAllowlist {
    pub fn new(nets: Vec<IpNet>) -> Self {
        Self(Arc::new(nets))
    }

    pub fn allows(&self, ip: IpAddr) -> bool {
        // Dual-stack sockets report IPv4 clients as ::ffff:a.b.c.d.
        let ip = ip.to_canonical();
        self.0.iter().any(|net| net.contains(&ip))
    }
}

/// Rejects clients outside the allowed networks. Uses the TCP peer address only;
/// forwarding headers are deliberately ignored because clients can forge them.
pub async fn ip_allowlist(
    State(allow): State<IpAllowlist>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    if allow.allows(peer.ip()) {
        next.run(req).await
    } else {
        tracing::warn!(client = %peer.ip(), "rejected client outside allowed networks");
        (StatusCode::FORBIDDEN, "forbidden").into_response()
    }
}

/// A path that has been verified to lie inside one of the configured roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPath {
    pub root_name: String,
    /// Canonical path of the root directory.
    pub root_path: PathBuf,
    /// Canonical requested path.
    pub path: PathBuf,
}

impl ResolvedPath {
    pub fn relative(&self, abs: &Path) -> Option<PathBuf> {
        abs.strip_prefix(&self.root_path)
            .ok()
            .map(Path::to_path_buf)
    }
}

/// Canonicalizes `requested` (resolving `..` and symlinks) and checks it is inside a root.
pub fn resolve_in_roots(roots: &[RootConfig], requested: &Path) -> Result<ResolvedPath, ApiError> {
    if !requested.is_absolute() {
        return Err(ApiError::BadRequest("path must be absolute".into()));
    }
    let canonical = match std::fs::canonicalize(requested) {
        Ok(p) => p,
        // Only report "not accessible" for paths under a root, so clients cannot
        // probe which paths exist elsewhere on the host.
        Err(e) if roots.iter().any(|r| requested.starts_with(&r.path)) => {
            return Err(ApiError::BadRequest(format!("path not accessible: {e}")));
        }
        Err(_) => return Err(forbidden()),
    };

    for root in roots {
        let Ok(root_canon) = std::fs::canonicalize(&root.path) else {
            tracing::warn!(root = %root.name, "configured root is not accessible (not mounted?)");
            continue;
        };
        if canonical.starts_with(&root_canon) {
            return Ok(ResolvedPath {
                root_name: root.name.clone(),
                root_path: root_canon,
                path: canonical,
            });
        }
    }
    Err(forbidden())
}

fn forbidden() -> ApiError {
    ApiError::Forbidden("path is not inside a configured index root".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_matches_private_and_mapped_v6() {
        let allow = IpAllowlist::new(vec!["192.168.0.0/16".parse().unwrap()]);
        assert!(allow.allows("192.168.178.20".parse().unwrap()));
        assert!(allow.allows("::ffff:192.168.1.1".parse().unwrap()));
        assert!(!allow.allows("10.0.0.1".parse().unwrap()));
        assert!(!allow.allows("8.8.8.8".parse().unwrap()));
    }

    fn roots(dir: &Path) -> Vec<RootConfig> {
        vec![RootConfig {
            name: "test".into(),
            path: dir.to_path_buf(),
        }]
    }

    #[test]
    fn resolves_inside_root() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        let r = resolve_in_roots(&roots(tmp.path()), &sub).unwrap();
        assert_eq!(r.root_name, "test");
        assert_eq!(r.relative(&r.path).unwrap(), PathBuf::from("a/b"));
    }

    #[test]
    fn rejects_dotdot_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let escape = root.join("..");
        assert!(matches!(
            resolve_in_roots(&roots(&root), &escape),
            Err(ApiError::Forbidden(_))
        ));
    }

    #[test]
    fn rejects_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        assert!(matches!(
            resolve_in_roots(&roots(&root), &root.join("link")),
            Err(ApiError::Forbidden(_))
        ));
    }

    #[test]
    fn rejects_relative_path() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            resolve_in_roots(&roots(tmp.path()), Path::new("relative")),
            Err(ApiError::BadRequest(_))
        ));
    }
}

use crate::backend::SandboxBackend;
use crate::coordinator::SessionCoordinator;
use crate::error::Result as CoreResult;
use crate::protocol::{Method, ProtocolError, Request, Response};
use serde_json::Value;
use std::path::Path;

/// Synchronous JSON-RPC dispatcher used by `deltabox serve` and tests.
pub struct Server {
    manager: SessionCoordinator,
}

impl Server {
    pub fn new(project: &Path, _backend: Box<dyn SandboxBackend>) -> CoreResult<Self> {
        Ok(Self {
            manager: SessionCoordinator::new(project, None)?,
        })
    }

    pub fn handle(&mut self, request: Request) -> Response {
        match self.dispatch(request.method) {
            Ok(value) => Response {
                id: request.id,
                result: Some(value),
                error: None,
            },
            Err(e) => Response {
                id: request.id,
                result: None,
                error: Some(ProtocolError {
                    code: -32000,
                    message: e.to_string(),
                }),
            },
        }
    }

    fn dispatch(&mut self, method: Method) -> CoreResult<Value> {
        let value = match method {
            Method::SessionStart(p) => serde_json::to_value(self.manager.start_with_durability(
                p.policy.as_deref(),
                p.fs_mode,
                p.require_fs_mode,
                p.durability,
                p.group_commit_ms,
            )?)?,
            Method::SessionStop(p) => serde_json::to_value(self.manager.stop(&p.session_id)?)?,
            Method::SessionStatus(p) => serde_json::to_value(self.manager.status(&p.session_id)?)?,
            Method::SessionExec(p) => serde_json::to_value(self.manager.exec(p)?)?,
            Method::FsRead(p) => {
                serde_json::to_value(self.manager.read_file(&p.session_id, &p.path)?)?
            }
            Method::FsWrite(p) => {
                use base64::Engine;
                let content = base64::engine::general_purpose::STANDARD
                    .decode(&p.content_base64)
                    .map_err(|e| crate::error::Error::InvalidPolicy(format!("bad base64: {e}")))?;
                self.manager.write_file(&p.session_id, &p.path, content)?;
                serde_json::json!(null)
            }
            Method::CheckpointCreate(p) => serde_json::to_value(self.manager.checkpoint(p)?)?,
            Method::DiffList(p) => serde_json::to_value(self.manager.diff(p)?)?,
            Method::ChangesCommit(p) => serde_json::to_value(self.manager.commit(p)?)?,
            Method::ChangesAbort(p) => serde_json::to_value(self.manager.abort(p)?)?,
            Method::ReviewBegin(p) => serde_json::to_value(self.manager.begin_review(p)?)?,
            Method::ReviewStatus(p) => {
                serde_json::to_value(self.manager.review_status(&p.session_id)?)?
            }
            Method::ReviewDiff(p) => {
                serde_json::to_value(self.manager.review_diff(&p.session_id)?)?
            }
            Method::ReviewCommit(p) => {
                serde_json::to_value(self.manager.review_commit(&p.session_id)?)?
            }
            Method::ReviewAbort(p) => {
                serde_json::to_value(self.manager.review_abort(&p.session_id)?)?
            }
            Method::ReviewSkip(p) => {
                serde_json::to_value(self.manager.review_skip(&p.session_id)?)?
            }
        };
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::DefaultBackend;
    use crate::protocol::{
        FsWriteParams, Request, ReviewBeginParams, ReviewKind, ReviewStatusParams,
        SessionStartParams, SessionStatusParams, SessionStopParams,
    };

    #[cfg(target_os = "linux")]
    fn default_backend() -> DefaultBackend {
        DefaultBackend::default()
    }

    #[cfg(target_os = "macos")]
    fn default_backend() -> DefaultBackend {
        DefaultBackend::default()
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn default_backend() -> DefaultBackend {
        DefaultBackend
    }

    #[test]
    fn server_session_lifecycle_over_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::new(dir.path(), Box::new(default_backend())).unwrap();

        let start = Request {
            id: 1,
            method: Method::SessionStart(SessionStartParams {
                project: dir.path().to_path_buf(),
                policy: None,
                fs_mode: Some(crate::protocol::FilesystemMode::CopyPrivate),
                require_fs_mode: false,
                durability: crate::protocol::DurabilityMode::Fast,
                group_commit_ms: None,
            }),
        };
        let response = server.handle(start);
        assert!(response.error.is_none());
        let result = response.result.unwrap();
        let session_id = result
            .get("session_id")
            .expect("session_id in result")
            .as_str()
            .unwrap()
            .to_string();

        let status = server.handle(Request {
            id: 2,
            method: Method::SessionStatus(SessionStatusParams {
                session_id: session_id.clone(),
            }),
        });
        assert!(status.error.is_none());

        let stop = server.handle(Request {
            id: 3,
            method: Method::SessionStop(SessionStopParams { session_id }),
        });
        assert!(stop.error.is_none());
    }

    #[test]
    fn server_restart_reopens_durable_session() {
        use base64::Engine;
        let dir = tempfile::tempdir().unwrap();
        let mut first = Server::new(dir.path(), Box::new(default_backend())).unwrap();
        let start = first.handle(Request {
            id: 1,
            method: Method::SessionStart(SessionStartParams {
                project: dir.path().to_path_buf(),
                policy: None,
                fs_mode: Some(crate::protocol::FilesystemMode::CopyPrivate),
                require_fs_mode: false,
                durability: crate::protocol::DurabilityMode::Fast,
                group_commit_ms: None,
            }),
        });
        let sid = start.result.unwrap()["session_id"]
            .as_str()
            .unwrap()
            .to_string();
        let write = first.handle(Request {
            id: 2,
            method: Method::FsWrite(FsWriteParams {
                session_id: sid.clone(),
                path: dir.path().join("pending.txt"),
                content_base64: base64::engine::general_purpose::STANDARD.encode("pending"),
            }),
        });
        assert!(write.error.is_none(), "write failed: {:?}", write.error);
        drop(first);

        let mut reopened = Server::new(dir.path(), Box::new(default_backend())).unwrap();
        let status = reopened.handle(Request {
            id: 3,
            method: Method::SessionStatus(SessionStatusParams { session_id: sid }),
        });
        assert!(status.error.is_none());
        assert_eq!(status.result.unwrap()["pending_changes"], 1);
        assert!(!dir.path().join("pending.txt").exists());
    }

    #[test]
    fn server_restart_preserves_active_review() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = Server::new(dir.path(), Box::new(default_backend())).unwrap();
        let start = first.handle(Request {
            id: 1,
            method: Method::SessionStart(SessionStartParams {
                project: dir.path().to_path_buf(),
                policy: None,
                fs_mode: Some(crate::protocol::FilesystemMode::CopyPrivate),
                require_fs_mode: false,
                durability: crate::protocol::DurabilityMode::Fast,
                group_commit_ms: None,
            }),
        });
        let sid = start.result.unwrap()["session_id"]
            .as_str()
            .unwrap()
            .to_string();
        let begin = first.handle(Request {
            id: 2,
            method: Method::ReviewBegin(ReviewBeginParams {
                session_id: sid.clone(),
                kind: ReviewKind::Task,
                label: Some("turn".into()),
            }),
        });
        let checkpoint = begin.result.unwrap()["checkpoint_id"]
            .as_str()
            .unwrap()
            .to_string();
        drop(first);

        let mut reopened = Server::new(dir.path(), Box::new(default_backend())).unwrap();
        let status = reopened.handle(Request {
            id: 3,
            method: Method::ReviewStatus(ReviewStatusParams { session_id: sid }),
        });
        assert_eq!(status.result.unwrap()["checkpoint_id"], checkpoint);
    }
}

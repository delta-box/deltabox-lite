use crate::backend::SandboxBackend;
use crate::error::Result as CoreResult;
use crate::protocol::{Method, ProtocolError, Request, Response};
use crate::session::SessionManager;
use serde_json::Value;
use std::path::Path;

/// Synchronous JSON-RPC dispatcher used by `deltabox serve` and tests.
pub struct Server {
    manager: SessionManager,
}

impl Server {
    pub fn new(project: &Path, backend: Box<dyn SandboxBackend>) -> CoreResult<Self> {
        Ok(Self {
            manager: SessionManager::new(project, backend)?,
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
            Method::SessionStart(p) => {
                serde_json::to_value(self.manager.start(p.policy.as_deref())?)?
            }
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
        };
        Ok(value)
    }
}

#[cfg(test)]
#[cfg(not(target_os = "linux"))]
mod tests {
    use super::*;
    use crate::backend::stub::StubBackend;
    use crate::protocol::{Request, SessionStartParams, SessionStatusParams, SessionStopParams};

    #[test]
    fn server_session_lifecycle_over_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::new(dir.path(), Box::new(StubBackend::default())).unwrap();

        let start = Request {
            id: 1,
            method: Method::SessionStart(SessionStartParams {
                project: dir.path().to_path_buf(),
                policy: None,
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
}

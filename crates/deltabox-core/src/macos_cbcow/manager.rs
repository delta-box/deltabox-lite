use super::state::{relative_string, Mode, State};
use crate::command_executor::{self, Endpoint as CommandEndpoint};
use crate::coordinator::CommandExecutionMode;
use crate::error::{Error, Result};
use crate::journal::{Journal, JournalEntry, JournalKind};
use crate::locks::Lock;
use crate::pathmap::PathMapper;
use crate::policy::Policy;
use crate::protocol::{
    AbortParams, AbortResult, ActiveReview, CheckpointInfo, CheckpointParams, CommitParams,
    CommitResult, DiffParams, DiffResult, DurabilityMode, ExecParams, ExecRequest, ExecResult,
    FilesystemInfo, FilesystemMode, FsReadResult, ReviewBeginParams, SessionInfo, SessionSpec,
    SessionStatus,
};
use crate::session::{now_ms, SessionMeta, SessionRegistry};
use crate::state_layout;
use base64::Engine;
use deltafs_fuse::macos::{
    current_event_id, device_uuid, event_helper_barrier, event_helper_boundary, stop_event_helper,
    EventBatch, EventStream,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

const RECORD_FILE: &str = "macos-native.json";
const SESSION_META: &str = "session.json";
const ACTIVE_REVIEW: &str = "active_review.json";
const SESSION_LOCK: &str = "session.lock";
const PROJECT_LEASE: &str = "active-cbcow";
const EVENT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_READ_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    state_root: PathBuf,
    durability: DurabilityMode,
}

pub struct Manager {
    project: PathBuf,
    root: PathBuf,
    helper_bin: PathBuf,
    execution_mode: CommandExecutionMode,
    state_base: PathBuf,
    embedded_fast: bool,
    fast_sessions: Mutex<HashMap<String, Arc<Mutex<FastSession>>>>,
}

struct FastSession {
    record: Record,
    meta: SessionMeta,
    policy: Policy,
    state: State,
    review: Option<ActiveReview>,
    journal: Journal,
    shell: Option<crate::trusted::PersistentBash>,
}

enum EventTracker {
    Helper,
    Stream(EventStream),
}

impl Manager {
    pub fn new(project: &Path, helper_bin: PathBuf) -> Result<Self> {
        Self::new_with_execution_mode(
            project,
            helper_bin,
            CommandExecutionMode::Sandboxed,
            None,
            false,
        )
    }

    pub fn new_with_execution_mode(
        project: &Path,
        helper_bin: PathBuf,
        execution_mode: CommandExecutionMode,
        state_base_override: Option<PathBuf>,
        embedded_fast: bool,
    ) -> Result<Self> {
        let project = project.canonicalize()?;
        let root = state_layout::state_root(&project)?;
        fs::create_dir_all(&root)?;
        let state_base = state_base_override
            .map(|path| path.join("macos-native"))
            .map_or_else(|| state_base(&project), Ok)?;
        Ok(Self {
            project,
            root,
            helper_bin,
            execution_mode,
            state_base,
            embedded_fast,
            fast_sessions: Mutex::new(HashMap::new()),
        })
    }

    pub fn start(
        &self,
        policy_path: Option<&Path>,
        mode: FilesystemMode,
        durability: DurabilityMode,
        group_commit_ms: Option<u64>,
    ) -> Result<SessionInfo> {
        if group_commit_ms.is_some() {
            return Err(Error::InvalidDurability(
                "group commit is unavailable for the macOS native provider".into(),
            ));
        }
        let mode = match mode {
            FilesystemMode::CbcowThrough => Mode::Through,
            FilesystemMode::CbcowPrivate => Mode::Private,
            FilesystemMode::CopyPrivate => {
                return Err(Error::Backend("invalid macOS CB-CoW mode".into()))
            }
        };
        // Policy resolution reads only per-request inputs; keeping it outside
        // the admission lock keeps policy IO out of the project-wide window.
        let policy_path = resolve_policy(&self.project, policy_path)?;
        let policy = match &policy_path {
            Some(path) => Policy::load(path)?,
            None => Policy::default_agent(),
        };
        if policy.network.allow_approval && self.execution_mode == CommandExecutionMode::Trusted {
            return Err(Error::Unsupported("network approvals require deltabox isolation".into()));
        }
        policy.validate()?;

        let session_id = Uuid::new_v4().to_string();
        let store = self.store(&session_id);
        let state_root = state_base(&self.project)?.join(&session_id);
        let started_at_ms = now_ms();
        // Admission critical section: reclaim/check/grant of the project
        // lease plus the durable Running record admission judges liveness by.
        // The record must be pinned before cbcow.lock drops — a sibling's
        // acquire would otherwise reclaim this fresh lease as dead, and a
        // through start could then take exclusivity while this start is still
        // scanning. Everything slow (host scans, helper spawn) runs after the
        // lock is released so concurrent starts overlap.
        let _start_pin;
        {
            let _project_lock = Lock::acquire(&self.root.join("cbcow.lock"))?;
            crate::project_lease::acquire(
                &crate::state_layout::leases_dir(&self.project)?,
                &self.root.join(PROJECT_LEASE),
                mode_for_lease(mode),
                &session_id,
                |sid| {
                    self.load_meta(sid)
                        .is_ok_and(|meta| meta.status == SessionStatus::Running)
                },
            )?;
            fs::create_dir_all(&store)?;
            // Held for the whole start, acquired before the Running record
            // exists: while this process is alive, `stop` reads the pin as
            // "start in flight" instead of rolling back a live admission; the
            // kernel drops it on death, which is what lets `stop` finalize a
            // killed start.
            _start_pin = Lock::acquire(&store.join("start.lock"))?;
            self.save_meta(&SessionMeta {
                session_id: session_id.clone(),
                project: self.project.clone(),
                policy_path: policy_path.clone(),
                started_at_ms,
                status: SessionStatus::Running,
                filesystem: FilesystemInfo::macos_native(mode_for_lease(mode))
                    .with_durability(durability, None),
            })?;
        }

        let started = (|| -> Result<SessionInfo> {
            fs::create_dir_all(&state_root)?;
            fs::set_permissions(&state_root, fs::Permissions::from_mode(0o700))?;
            // The probe source must live on the project's volume: clonefile only
            // works within one volume, and the session root now lives under the
            // external state base, which may be a different volume. The in-project
            // .deltabox directory is still required for the FSEvents barrier and
            // is filtered from scans, so it doubles as the probe scratch area.
            let probe_dir = self.project.join(".deltabox");
            fs::create_dir_all(&probe_dir)?;
            probe_clonefile(&probe_dir, &state_root, &session_id)?;
            write_file(
                &state_root.join("command-token"),
                format!("{}{}\n", Uuid::new_v4(), Uuid::new_v4()).as_bytes(),
                durability == DurabilityMode::Strict,
            )?;

            let durable = durability == DurabilityMode::Strict;
            let event_id = current_event_id();
            let uuid = device_uuid(&self.project)?;
            // The accepted base and private live tree are two scans of the
            // host, taken under the publish lock in SHARED mode: sibling
            // starts scan concurrently, while publication and publication
            // recovery — exclusive holders — are still fully excluded. No
            // sibling publication can land between the scans or before the
            // event boundary pins the baseline, exactly as under the old
            // exclusive hold.
            let publish =
                Lock::acquire_shared(&crate::state_layout::publish_lock_path(&self.project)?)?;
            crate::test_hooks::maybe_pause("start_scan_shared");
            let mut state = State::create(
                &self.project,
                &state_root.join("engine"),
                mode,
                durable,
                event_id,
                uuid.clone(),
            )?;
            state.set_event_boundary(current_event_id(), uuid)?;
            fs::write(
                store.join("publog.seq"),
                format!("{}\n", crate::publog::head_seq(&self.project)?),
            )?;
            drop(publish);
            self.prepare_events(&mut state, true)?;

            let record = Record {
                state_root: state_root.clone(),
                durability,
            };
            save_json(&store.join(RECORD_FILE), &record, durable)?;
            let filesystem = filesystem_info(mode, &state, durability);
            let meta = SessionMeta {
                session_id: session_id.clone(),
                project: self.project.clone(),
                policy_path: policy_path.clone(),
                started_at_ms,
                status: SessionStatus::Running,
                filesystem: filesystem.clone(),
            };
            self.save_meta(&meta)?;
            if self.embedded_fast && self.execution_mode == CommandExecutionMode::Trusted {
                let mut journal = Journal::open(&store.join("journal.jsonl"))?;
                append_journal(
                    &mut journal,
                    &record,
                    &session_id,
                    JournalKind::SessionStart,
                    None,
                )?;
                self.fast_sessions
                    .lock()
                    .expect("embedded session map poisoned")
                    .insert(
                        session_id.clone(),
                        Arc::new(Mutex::new(FastSession {
                            record,
                            meta,
                            policy,
                            state,
                            review: None,
                            journal,
                            shell: None,
                        })),
                    );
            } else {
                self.append(&record, &session_id, JournalKind::SessionStart, None)?;
            }
            Ok(SessionInfo {
                session_id: session_id.clone(),
                project: self.project.clone(),
                started_at_ms,
                status: SessionStatus::Running,
                checkpoint_count: 0,
                pending_changes: 0,
                filesystem,
            })
        })();
        if started.is_err() {
            // Admission rollback: mark the record dead first so any racing
            // acquire stops treating the lease as live, then drop the lease.
            // release() without cbcow.lock matches stop(); a concurrent
            // reclaim of the now-Stopped record is the intended outcome.
            if let Ok(mut meta) = self.load_meta(&session_id) {
                meta.status = SessionStatus::Stopped;
                let _ = self.save_meta(&meta);
            }
            if let Ok(leases) = crate::state_layout::leases_dir(&self.project) {
                crate::project_lease::release(&leases, &self.root.join(PROJECT_LEASE), &session_id);
            }
            let _ = fs::remove_dir_all(&state_root);
        }
        started
    }

    pub fn status(&self, session_id: &str) -> Result<SessionInfo> {
        if !self.store(session_id).join(RECORD_FILE).exists() {
            // Start in flight (or torn): report the durable record as-is
            // instead of failing on the missing engine.
            let meta = self.load_meta(session_id)?;
            if meta.status == SessionStatus::Stopped {
                return Ok(info_from_stopped(meta));
            }
            return Ok(SessionInfo {
                session_id: meta.session_id,
                project: meta.project,
                started_at_ms: meta.started_at_ms,
                status: meta.status,
                checkpoint_count: 0,
                pending_changes: 0,
                filesystem: meta.filesystem,
            });
        }
        if let Some(fast) = self.fast_session(session_id) {
            let session = fast.lock().expect("embedded session poisoned");
            if session.meta.status == SessionStatus::Stopped {
                return Ok(info_from_stopped(session.meta.clone()));
            }
            self.enforce_fast_storage_limit(&session)?;
            let status = session.state.status();
            return Ok(SessionInfo {
                session_id: session.meta.session_id.clone(),
                project: session.meta.project.clone(),
                started_at_ms: session.meta.started_at_ms,
                status: session.meta.status.clone(),
                checkpoint_count: status.checkpoints,
                pending_changes: status.pending,
                filesystem: filesystem_info(status.mode, &session.state, session.record.durability),
            });
        }
        let _lock = self.lock(session_id)?;
        let mut meta = self.load_meta(session_id)?;
        if meta.status == SessionStatus::Stopped {
            return Ok(info_from_stopped(meta));
        }
        let record = self.record(session_id)?;
        let mut state = self.open_state(&record)?;
        self.reconcile_history(&mut state)?;
        self.enforce_storage_limit(&meta, &state)?;
        let status = state.status();
        let filesystem = filesystem_info(status.mode, &state, record.durability);
        if meta.filesystem != filesystem {
            meta.filesystem = filesystem.clone();
            self.save_meta(&meta)?;
        }
        Ok(SessionInfo {
            session_id: meta.session_id,
            project: meta.project,
            started_at_ms: meta.started_at_ms,
            status: meta.status,
            checkpoint_count: status.checkpoints,
            pending_changes: status.pending,
            filesystem,
        })
    }

    pub fn stop(&self, session_id: &str) -> Result<SessionInfo> {
        if !self.store(session_id).join(RECORD_FILE).exists() {
            let mut meta = self.running_meta(session_id)?;
            // No engine record: the start is either still scanning or died
            // before writing it. The admission pin decides which — a live
            // start holds it; a dead one's was released by the kernel.
            let Some(_pin) =
                Lock::try_acquire(&self.store(session_id).join("start.lock"))?
            else {
                return Err(Error::SessionBusy(session_id.into()));
            };
            if !self.store(session_id).join(RECORD_FILE).exists() {
                // Torn start: finalize the rollback the dead start never ran,
                // so its lease stops pinning project admission.
                meta.status = SessionStatus::Stopped;
                self.save_meta(&meta)?;
                if let Ok(leases) = crate::state_layout::leases_dir(&self.project) {
                    crate::project_lease::release(
                        &leases,
                        &self.root.join(PROJECT_LEASE),
                        session_id,
                    );
                }
                if let Ok(base) = state_base(&self.project) {
                    let _ = fs::remove_dir_all(base.join(session_id));
                }
                return Ok(info_from_stopped(meta));
            }
            // The start completed while we probed; run the normal stop.
        }
        if let Some(fast) = self.fast_session(session_id) {
            let info = {
                let mut session = fast.lock().expect("embedded session poisoned");
                self.require_fast_running(&session)?;
                if session.state.status().pending != 0 {
                    session.state.abort(None)?;
                    session.append(session_id, JournalKind::Abort, None)?;
                }
                let _ = command_executor::shutdown(&command_endpoint(&session.record));
                let _ = stop_event_helper(&session.state.event_socket());
                session.review = None;
                self.save_review(session_id, None)?;
                session.meta.status = SessionStatus::Stopped;
                session.meta.filesystem = filesystem_info(
                    session.state.status().mode,
                    &session.state,
                    session.record.durability,
                );
                self.save_meta(&session.meta)?;
                if fs::read_to_string(self.root.join(PROJECT_LEASE))
                    .is_ok_and(|value| value.trim() == session_id)
                {
                    fs::remove_file(self.root.join(PROJECT_LEASE))?;
                }
                session.append(session_id, JournalKind::SessionStop, None)?;
                info_from_stopped(session.meta.clone())
            };
            let mut sessions = self
                .fast_sessions
                .lock()
                .expect("embedded session map poisoned");
            if sessions
                .get(session_id)
                .is_some_and(|current| Arc::ptr_eq(current, &fast))
            {
                sessions.remove(session_id);
            }
            command_executor::cleanup_stopped(&command_endpoint(&self.record(session_id)?));
            return Ok(info);
        }
        let _lock = self.lock(session_id)?;
        let mut meta = self.running_meta(session_id)?;
        let record = self.record(session_id)?;
        let mut state = self.open_state(&record)?;
        self.reconcile_history(&mut state)?;
        if state.status().pending != 0 {
            self.mutate_live(&mut state, |state| state.abort(None).map(|_| ()))?;
            self.append(&record, session_id, JournalKind::Abort, None)?;
        }
        command_executor::shutdown(&command_endpoint(&record))?;
        let _ = stop_event_helper(&state.event_socket());
        self.save_review(session_id, None)?;
        meta.status = SessionStatus::Stopped;
        meta.filesystem = filesystem_info(state.status().mode, &state, record.durability);
        self.save_meta(&meta)?;
        crate::project_lease::release(
            &crate::state_layout::leases_dir(&self.project)?,
            &self.root.join(PROJECT_LEASE),
            session_id,
        );
        self.append(&record, session_id, JournalKind::SessionStop, None)?;
        command_executor::cleanup_stopped(&command_endpoint(&record));
        Ok(info_from_stopped(meta))
    }

    pub fn remove_stopped(&self, session_id: &str) -> Result<()> {
        let lock = self.exec_lock(session_id)?;
        let meta = self.load_meta(session_id)?;
        if meta.status != SessionStatus::Stopped {
            return Err(Error::SessionBusy(session_id.into()));
        }
        let record = self.record(session_id)?;
        let runtime = command_endpoint(&record)
            .socket
            .parent()
            .expect("command socket has a runtime directory")
            .to_path_buf();
        SessionRegistry::update(&self.root, |registry| registry.remove(session_id))?;
        drop(lock);

        if runtime.exists() {
            fs::remove_dir_all(runtime)?;
        }
        fs::remove_dir_all(record.state_root)?;
        fs::remove_dir_all(self.store(session_id))?;
        Ok(())
    }

    pub fn exec(&self, params: ExecParams) -> Result<ExecResult> {
        let ExecParams {
            session_id,
            argv,
            cwd,
            env,
            label,
            execution_profile,
            network_allow,
        } = params;
        if !network_allow.is_empty() && self.execution_mode == CommandExecutionMode::Trusted {
            return Err(Error::Unsupported(
                "network grants require deltabox isolation".into(),
            ));
        }
        if let Some(fast) = self.fast_session(&session_id) {
            return self.exec_fast(fast, session_id, argv, cwd, env, label);
        }
        let _lock = self.exec_lock(&session_id)?;
        let meta = self.running_meta(&session_id)?;
        let record = self.record(&session_id)?;
        let mut operation = match self.execution_mode {
            CommandExecutionMode::Sandboxed => Some(command_executor::lock_operations(
                &command_endpoint(&record),
            )?),
            CommandExecutionMode::Trusted => None,
        };
        let mut state = self.open_state(&record)?;
        let tracker = if state.durable() {
            Some(self.prepare_events(&mut state, false)?)
        } else {
            None
        };
        if state.durable() {
            self.enforce_storage_limit(&meta, &state)?;
        }
        let result = match self.execution_mode {
            CommandExecutionMode::Trusted => {
                let live = state.live();
                let mapper = PathMapper::new(&self.project, &live);
                crate::trusted::exec(ExecRequest {
                    argv: argv
                        .into_iter()
                        .map(|value| mapper.map_arg(&value))
                        .collect(),
                    cwd: Some(
                        cwd.as_deref()
                            .map(|path| mapper.map_path(path))
                            .unwrap_or(live),
                    ),
                    env: env
                        .map(|values| {
                            values
                                .into_iter()
                                .map(|(key, value)| {
                                    mapper.map_env(&key, &value).map(|value| (key, value))
                                })
                                .collect::<Result<Vec<_>>>()
                        })
                        .transpose()?,
                })
            }
            CommandExecutionMode::Sandboxed => {
                let mut policy = self.policy(&meta)?;
                policy.apply_execution_profile(execution_profile);
                policy.apply_network_allow(&network_allow)?;
                // Through mode tracks writes in the real project; only private
                // workspaces must keep commands away from the backing tree.
                if state.status().mode == Mode::Private {
                    execution_profile
                        .ensure_backing_project_is_deniable(&state.live(), &self.project)?;
                }
                let endpoint = command_endpoint(&record);
                if !policy
                    .filesystem
                    .deny
                    .iter()
                    .any(|value| value == ".deltabox/")
                {
                    policy.filesystem.deny.push(".deltabox/".into());
                }
                let spec = SessionSpec {
                    project: state.live(),
                    host_project: Some(self.project.clone()),
                    store_dir: record.state_root.join("runtime"),
                    policy,
                    resolved_deny_paths: Some(vec![
                        self.root.clone(),
                        record.state_root.clone(),
                        endpoint
                            .socket
                            .parent()
                            .expect("command runtime directory")
                            .into(),
                        state.event_socket(),
                    ]),
                };
                let request = ExecRequest {
                    argv,
                    cwd,
                    env: command_executor::capture_environment(env),
                };
                if !endpoint.token_file.exists() {
                    write_file(
                        &endpoint.token_file,
                        format!("{}{}\n", Uuid::new_v4(), Uuid::new_v4()).as_bytes(),
                        record.durability == DurabilityMode::Strict,
                    )?;
                }
                command_executor::ensure_running(
                    &endpoint,
                    &self.helper_bin,
                    &record.state_root.join("command-helper.log"),
                )?;
                drop(operation.take());
                command_executor::exec(&endpoint, &session_id, 0, spec, request)
            }
        };
        let tracked = tracker.as_ref().map_or(Ok(()), |tracker| {
            self.finish_events(&mut state, tracker, true)
        });
        let result = result?;
        tracked?;
        if tracker.is_some() {
            self.enforce_storage_limit(&meta, &state)?;
        }
        self.append(
            &record,
            &session_id,
            JournalKind::Exec,
            Some(serde_json::json!({"label": label, "result": result})),
        )?;
        Ok(result)
    }

    pub fn read_file(&self, session_id: &str, path: &Path) -> Result<FsReadResult> {
        let bytes = self.read_file_bytes(session_id, path)?;
        Ok(FsReadResult {
            content_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
            size: bytes.len(),
        })
    }

    pub fn read_file_bytes(&self, session_id: &str, path: &Path) -> Result<Vec<u8>> {
        if let Some(fast) = self.fast_session(session_id) {
            let session = fast.lock().expect("embedded session poisoned");
            self.require_fast_running(&session)?;
            let relative = relative_path(&self.project, path)?;
            if session.policy.path_is_denied(&relative) {
                return Err(Error::Backend("path denied by policy".into()));
            }
            let target = checked_path(&session.state.live(), &relative)?;
            let metadata = fs::metadata(&target)?;
            if !metadata.is_file() || metadata.len() > MAX_READ_BYTES {
                return Err(Error::Backend(
                    "fs.read target is unavailable or too large".into(),
                ));
            }
            return Ok(fs::read(target)?);
        }
        let _lock = self.lock(session_id)?;
        let meta = self.running_meta(session_id)?;
        let record = self.record(session_id)?;
        let mut state = self.open_state(&record)?;
        self.reconcile_history(&mut state)?;
        let relative = relative_path(&self.project, path)?;
        if self.policy(&meta)?.path_is_denied(&relative) {
            return Err(Error::Backend("path denied by policy".into()));
        }
        let target = checked_path(&state.live(), &relative)?;
        let metadata = fs::metadata(&target)?;
        if !metadata.is_file() || metadata.len() > MAX_READ_BYTES {
            return Err(Error::Backend(
                "fs.read target is unavailable or too large".into(),
            ));
        }
        Ok(fs::read(target)?)
    }

    pub fn write_file(&self, session_id: &str, path: &Path, content: Vec<u8>) -> Result<()> {
        self.write_file_bytes(session_id, path, &content)
    }

    pub fn write_file_bytes(&self, session_id: &str, path: &Path, content: &[u8]) -> Result<()> {
        if let Some(fast) = self.fast_session(session_id) {
            let mut session = fast.lock().expect("embedded session poisoned");
            self.require_fast_running(&session)?;
            let relative = relative_path(&self.project, path)?;
            self.validate_fast_write(&session, &relative, content.len())?;
            let target = checked_path(&session.state.live(), &relative)?;
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(target, content)?;
            session.state.mark_paths([relative_string(&relative)?])?;
            self.enforce_fast_storage_limit(&session)?;
            session.append(
                session_id,
                JournalKind::Write,
                Some(serde_json::json!({"path": relative})),
            )?;
            return Ok(());
        }
        let _lock = self.lock(session_id)?;
        let meta = self.running_meta(session_id)?;
        let record = self.record(session_id)?;
        let mut state = self.open_state(&record)?;
        let relative = relative_path(&self.project, path)?;
        let policy = self.policy(&meta)?;
        if policy.path_is_denied(&relative) {
            return Err(Error::Backend("path denied by policy".into()));
        }
        if policy
            .limits
            .max_write_bytes
            .is_some_and(|limit| content.len() as u64 > limit)
        {
            return Err(Error::Backend("fs.write exceeds max_write_bytes".into()));
        }
        let stream = self.prepare_events(&mut state, true)?;
        let target = checked_path(&state.live(), &relative)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(target, content)?;
        self.finish_events(&mut state, &stream, true)?;
        self.enforce_storage_limit(&meta, &state)?;
        self.append(
            &record,
            session_id,
            JournalKind::Write,
            Some(serde_json::json!({"path": relative})),
        )
    }

    pub fn sync(&self, session_id: &str) -> Result<crate::protocol::SyncResult> {
        let _lock = self.lock(session_id)?;
        self.running_meta(session_id)?;
        let record = self.record(session_id)?;
        let mut state = self.open_state(&record)?;
        let empty = crate::protocol::SyncResult {
            folded_publications: 0,
            stale_paths: Vec::new(),
            conflicts: Vec::new(),
        };
        if state.status().mode != Mode::Private {
            // Through mode has no private baseline: its view is the host.
            // docs/protocol.md declares sync cbcow-private only, so reject
            // loudly instead of masking misdirected client calls.
            return Err(Error::Unsupported(
                "session.sync requires a cbcow-private session".into(),
            ));
        }
        let _publish =
            Lock::acquire(&crate::state_layout::publish_lock_path(&self.project)?)?;
        let seen = fs::read_to_string(self.store(session_id).join("publog.seq"))
            .ok()
            .and_then(|raw| raw.trim().parse().ok())
            .unwrap_or(0);
        let entries = crate::publog::entries_since(&self.project, seen)?;
        if entries.is_empty() {
            return Ok(empty);
        }
        let mine = state.pending_set();
        let mut latest: std::collections::BTreeMap<String, crate::publog::PublogChange> =
            std::collections::BTreeMap::new();
        for entry in &entries {
            for change in &entry.changes {
                latest.insert(change.path.clone(), change.clone());
            }
        }
        let mut stale_paths = Vec::new();
        let mut conflicts = Vec::new();
        let stream = self.prepare_events(&mut state, true)?;
        for (path, change) in &latest {
            if !mine.contains(path) {
                stale_paths.push(path.clone());
                continue;
            }
            let host_path = self.project.join(path);
            let theirs = if host_path.is_file() {
                let theirs_rel = format!("{path}{}", crate::publog::THEIRS_DRAFT_SUFFIX);
                let target = state.live().join(&theirs_rel);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(target, fs::read(&host_path)?)?;
                Some(theirs_rel)
            } else {
                None
            };
            conflicts.push(crate::protocol::SyncConflict {
                path: path.clone(),
                host_kind: change.kind,
                theirs,
            });
        }
        self.finish_events(&mut state, &stream, true)?;
        let head = crate::publog::head_seq(&self.project)?;
        fs::write(
            self.store(session_id).join("publog.seq"),
            format!("{head}\n"),
        )?;
        Ok(crate::protocol::SyncResult {
            folded_publications: entries.len() as u64,
            stale_paths,
            conflicts,
        })
    }

    /// Register exact paths already written through the trusted mountpoint.
    pub fn record_file_changes(&self, session_id: &str, paths: &[PathBuf]) -> Result<()> {
        let fast = self.fast_session(session_id).ok_or_else(|| {
            Error::Backend("direct mount tracking requires embedded trusted mode".into())
        })?;
        let mut session = fast.lock().expect("embedded session poisoned");
        self.require_fast_running(&session)?;
        let mut relative = Vec::with_capacity(paths.len());
        let mut denied = false;
        for path in paths {
            let path = relative_path(&self.project, path)?;
            if session.policy.path_is_denied(&path) {
                denied = true;
            }
            checked_path(&session.state.live(), &path)?;
            relative.push(path);
        }
        session.state.mark_paths(
            relative
                .iter()
                .map(|path| relative_string(path))
                .collect::<Result<Vec<_>>>()?,
        )?;
        self.enforce_fast_storage_limit(&session)?;
        session.append(
            session_id,
            JournalKind::Write,
            Some(serde_json::json!({"paths": relative})),
        )?;
        if denied {
            // The direct writer has already mutated the live tree. Keep the
            // path tracked so review.abort can still restore it before
            // surfacing the policy error to the caller.
            return Err(Error::Backend("path denied by policy".into()));
        }
        Ok(())
    }

    pub fn checkpoint(&self, params: CheckpointParams) -> Result<CheckpointInfo> {
        if let Some(fast) = self.fast_session(&params.session_id) {
            let mut session = fast.lock().expect("embedded session poisoned");
            return self.checkpoint_fast_locked(&mut session, params);
        }
        let _lock = self.lock(&params.session_id)?;
        self.checkpoint_locked(params)
    }

    fn checkpoint_locked(&self, params: CheckpointParams) -> Result<CheckpointInfo> {
        self.running_meta(&params.session_id)?;
        let record = self.record(&params.session_id)?;
        let mut state = self.open_state(&record)?;
        let mut result = None;
        self.mutate_live(&mut state, |state| {
            result = Some(state.checkpoint(params.label.clone())?);
            Ok(())
        })?;
        let (id, changed) = result.expect("checkpoint result");
        self.append(&record, &params.session_id, JournalKind::Checkpoint, None)?;
        Ok(CheckpointInfo {
            id: id.to_string(),
            label: params.label,
            created_at_ms: now_ms(),
            change_count: changed,
            committed: false,
        })
    }

    pub fn diff(&self, params: DiffParams) -> Result<DiffResult> {
        if let Some(fast) = self.fast_session(&params.session_id) {
            let mut session = fast.lock().expect("embedded session poisoned");
            return self.diff_fast_locked(&mut session, params);
        }
        let _lock = self.lock(&params.session_id)?;
        self.diff_locked(params)
    }

    fn diff_locked(&self, params: DiffParams) -> Result<DiffResult> {
        let meta = self.running_meta(&params.session_id)?;
        let record = self.record(&params.session_id)?;
        let mut state = self.open_state(&record)?;
        self.reconcile_history(&mut state)?;
        self.enforce_storage_limit(&meta, &state)?;
        let from = parse_checkpoint(params.from.as_deref())?;
        let to = parse_checkpoint(params.to.as_deref())?;
        let changes = state.diff(from, to)?;
        self.append(&record, &params.session_id, JournalKind::Diff, None)?;
        Ok(DiffResult {
            from: params.from,
            to: params.to,
            pending_changes: changes.len(),
            changes,
        })
    }

    pub fn commit(&self, params: CommitParams) -> Result<CommitResult> {
        if let Some(fast) = self.fast_session(&params.session_id) {
            let mut session = fast.lock().expect("embedded session poisoned");
            return self.commit_fast_locked(&mut session, params);
        }
        let _lock = self.lock(&params.session_id)?;
        self.commit_locked(params)
    }

    fn commit_locked(&self, params: CommitParams) -> Result<CommitResult> {
        let meta = self.running_meta(&params.session_id)?;
        let record = self.record(&params.session_id)?;
        // open_state may take the publish lock itself to recover a torn
        // transaction, so it must complete before this commit's own lock:
        // flock does not re-enter within one process.
        let mut state = self.open_state(&record)?;
        // Publication window: staging, host application, and manifest update
        // must be atomic against sibling publications and recoveries. A
        // sibling landing between recovery and here is caught by validation.
        let _publish =
            Lock::acquire(&crate::state_layout::publish_lock_path(&self.project)?)?;
        crate::test_hooks::maybe_pause("publish_lock_held");
        let target = parse_checkpoint(params.checkpoint.as_deref())?;
        let tracker = self.prepare_events(&mut state, true)?;
        self.enforce_storage_limit(&meta, &state)?;
        let (seen, folded) = self.publog_cursor(&params.session_id)?;
        let endorsed_until = endorsement_bound(&params, seen);
        let result = state.commit(target, endorsed_until)?;
        self.finish_events(&mut state, &tracker, false)?;
        let (id, published) = result;
        let applied = published.len();
        self.record_publication(&params.session_id, &published)?;
        self.save_review(&params.session_id, None)?;
        self.append(&record, &params.session_id, JournalKind::Commit, None)?;
        Ok(CommitResult {
            checkpoint_id: id.to_string(),
            applied,
            folded_publications: (folded > 0).then_some(folded),
        })
    }

    /// The session's publog cursor (`seen`) and how many sibling publications
    /// landed after it. Read under the publish lock so the count is exact.
    fn publog_cursor(&self, session_id: &str) -> Result<(u64, u64)> {
        let seen = fs::read_to_string(self.store(session_id).join("publog.seq"))
            .ok()
            .and_then(|raw| raw.trim().parse().ok())
            .unwrap_or(0);
        let folded = crate::publog::entries_since(&self.project, seen)?.len() as u64;
        Ok((seen, folded))
    }

    /// Append this commit's publication to the shared publog and advance the
    /// session's cursor past it. Must run under the publish lock, after the
    /// host already carries the published files (their digests are sampled
    /// from the host). A commit that published nothing leaves the log alone.
    fn record_publication(
        &self,
        session_id: &str,
        published: &[crate::publog::PublogChange],
    ) -> Result<()> {
        if published.is_empty() {
            return Ok(());
        }
        let mut digests = std::collections::BTreeMap::new();
        for change in published {
            if change.kind == crate::changeset::ChangeKind::Deleted {
                continue;
            }
            let host_path = self.project.join(&change.path);
            if host_path.is_file() {
                if let Some(digest) = crate::publog::file_digest_capped(&host_path)? {
                    digests.insert(change.path.clone(), digest);
                }
            }
        }
        let seq = crate::publog::append(&self.project, session_id, published, digests)?;
        fs::write(
            self.store(session_id).join("publog.seq"),
            format!("{seq}\n"),
        )?;
        Ok(())
    }

    pub fn abort(&self, params: AbortParams) -> Result<AbortResult> {
        if let Some(fast) = self.fast_session(&params.session_id) {
            let mut session = fast.lock().expect("embedded session poisoned");
            return self.abort_fast_locked(&mut session, params);
        }
        let _lock = self.lock(&params.session_id)?;
        self.abort_locked(params)
    }

    fn abort_locked(&self, params: AbortParams) -> Result<AbortResult> {
        self.running_meta(&params.session_id)?;
        let record = self.record(&params.session_id)?;
        let mut state = self.open_state(&record)?;
        let target = parse_checkpoint(params.checkpoint.as_deref())?;
        let mut result = None;
        self.mutate_live(&mut state, |state| {
            result = Some(state.abort(target)?);
            Ok(())
        })?;
        let (id, restored) = result.expect("abort result");
        self.append(&record, &params.session_id, JournalKind::Abort, None)?;
        Ok(AbortResult {
            checkpoint_id: id.to_string(),
            restored,
        })
    }

    pub fn begin_review(&self, params: ReviewBeginParams) -> Result<ActiveReview> {
        if let Some(fast) = self.fast_session(&params.session_id) {
            let mut session = fast.lock().expect("embedded session poisoned");
            self.require_fast_running(&session)?;
            if let Some(active) = &session.review {
                return Ok(active.clone());
            }
            let checkpoint = self.checkpoint_fast_locked(
                &mut session,
                CheckpointParams {
                    session_id: params.session_id.clone(),
                    label: params.label.clone(),
                },
            )?;
            let active = ActiveReview {
                checkpoint_id: checkpoint.id,
                kind: params.kind,
                label: params.label,
                started_at_ms: now_ms(),
            };
            session.review = Some(active.clone());
            self.save_review(&params.session_id, Some(&active))?;
            session.append(&params.session_id, JournalKind::ReviewBegin, None)?;
            return Ok(active);
        }
        let _lock = self.lock(&params.session_id)?;
        self.running_meta(&params.session_id)?;
        if let Some(active) = self.review(&params.session_id)? {
            return Ok(active);
        }
        let checkpoint = self.checkpoint_locked(CheckpointParams {
            session_id: params.session_id.clone(),
            label: params.label.clone(),
        })?;
        let active = ActiveReview {
            checkpoint_id: checkpoint.id,
            kind: params.kind,
            label: params.label,
            started_at_ms: now_ms(),
        };
        self.save_review(&params.session_id, Some(&active))?;
        self.append(
            &self.record(&params.session_id)?,
            &params.session_id,
            JournalKind::ReviewBegin,
            None,
        )?;
        Ok(active)
    }

    pub fn review_status(&self, session_id: &str) -> Result<Option<ActiveReview>> {
        if let Some(fast) = self.fast_session(session_id) {
            let session = fast.lock().expect("embedded session poisoned");
            self.require_fast_running(&session)?;
            return Ok(session.review.clone());
        }
        let _lock = self.lock(session_id)?;
        self.running_meta(session_id)?;
        self.review(session_id)
    }

    pub fn review_diff(&self, session_id: &str) -> Result<DiffResult> {
        if let Some(fast) = self.fast_session(session_id) {
            let mut session = fast.lock().expect("embedded session poisoned");
            self.require_fast_running(&session)?;
            let active = session
                .review
                .clone()
                .ok_or_else(|| Error::ReviewNotActive(session_id.into()))?;
            return self.diff_fast_locked(
                &mut session,
                DiffParams {
                    session_id: session_id.into(),
                    from: Some(active.checkpoint_id),
                    to: None,
                },
            );
        }
        let _lock = self.lock(session_id)?;
        self.running_meta(session_id)?;
        let active = self.require_review(session_id)?;
        self.diff_locked(DiffParams {
            session_id: session_id.into(),
            from: Some(active.checkpoint_id),
            to: None,
        })
    }

    pub fn review_commit(&self, session_id: &str) -> Result<CommitResult> {
        if let Some(fast) = self.fast_session(session_id) {
            let mut session = fast.lock().expect("embedded session poisoned");
            self.require_fast_running(&session)?;
            if session.review.is_none() {
                return Err(Error::ReviewNotActive(session_id.into()));
            }
            return self.commit_fast_locked(
                &mut session,
                CommitParams {
                    live_processes: Default::default(),
                    session_id: session_id.into(),
                    checkpoint: None,
                    ours: false,
                },
            );
        }
        let _lock = self.lock(session_id)?;
        self.running_meta(session_id)?;
        self.require_review(session_id)?;
        self.commit_locked(CommitParams {
            live_processes: Default::default(),
            session_id: session_id.into(),
            checkpoint: None,
            ours: false,
        })
    }

    pub fn review_abort(&self, session_id: &str) -> Result<AbortResult> {
        if let Some(fast) = self.fast_session(session_id) {
            let mut session = fast.lock().expect("embedded session poisoned");
            self.require_fast_running(&session)?;
            let active = session
                .review
                .clone()
                .ok_or_else(|| Error::ReviewNotActive(session_id.into()))?;
            let result = self.abort_fast_locked(
                &mut session,
                AbortParams {
                    live_processes: Default::default(),
                    session_id: session_id.into(),
                    checkpoint: Some(active.checkpoint_id),
                },
            )?;
            session.review = None;
            self.save_review(session_id, None)?;
            return Ok(result);
        }
        let _lock = self.lock(session_id)?;
        self.running_meta(session_id)?;
        let active = self.require_review(session_id)?;
        let result = self.abort_locked(AbortParams {
            live_processes: Default::default(),
            session_id: session_id.into(),
            checkpoint: Some(active.checkpoint_id),
        })?;
        self.save_review(session_id, None)?;
        Ok(result)
    }

    pub fn review_skip(&self, session_id: &str) -> Result<ActiveReview> {
        if let Some(fast) = self.fast_session(session_id) {
            let mut session = fast.lock().expect("embedded session poisoned");
            self.require_fast_running(&session)?;
            let active = session
                .review
                .clone()
                .ok_or_else(|| Error::ReviewNotActive(session_id.into()))?;
            session.append(session_id, JournalKind::ReviewSkip, None)?;
            return Ok(active);
        }
        let _lock = self.lock(session_id)?;
        self.running_meta(session_id)?;
        let active = self.require_review(session_id)?;
        self.append(
            &self.record(session_id)?,
            session_id,
            JournalKind::ReviewSkip,
            None,
        )?;
        Ok(active)
    }

    fn fast_session(&self, session_id: &str) -> Option<Arc<Mutex<FastSession>>> {
        self.fast_sessions
            .lock()
            .expect("embedded session map poisoned")
            .get(session_id)
            .cloned()
    }

    fn require_fast_running(&self, session: &FastSession) -> Result<()> {
        if session.meta.status == SessionStatus::Running {
            Ok(())
        } else {
            Err(Error::SessionStopped(session.meta.session_id.clone()))
        }
    }

    fn validate_fast_write(
        &self,
        session: &FastSession,
        relative: &Path,
        content_len: usize,
    ) -> Result<()> {
        if session.policy.path_is_denied(relative) {
            return Err(Error::Backend("path denied by policy".into()));
        }
        if session
            .policy
            .limits
            .max_write_bytes
            .is_some_and(|limit| content_len as u64 > limit)
        {
            return Err(Error::Backend("fs.write exceeds max_write_bytes".into()));
        }
        Ok(())
    }

    fn enforce_fast_storage_limit(&self, session: &FastSession) -> Result<()> {
        let Some(limit) = session.policy.limits.max_storage_bytes else {
            return Ok(());
        };
        let used = session
            .state
            .diff(None, None)?
            .iter()
            .filter_map(|change| change.new_size)
            .sum::<u64>();
        if used > limit {
            return Err(Error::Backend(format!(
                "pending changes use {used} bytes, exceeding max_storage_bytes"
            )));
        }
        Ok(())
    }

    fn exec_fast(
        &self,
        fast: Arc<Mutex<FastSession>>,
        session_id: String,
        argv: Vec<String>,
        cwd: Option<PathBuf>,
        env: Option<Vec<(String, String)>>,
        label: Option<String>,
    ) -> Result<ExecResult> {
        let mut session = fast.lock().expect("embedded session poisoned");
        self.require_fast_running(&session)?;
        let live = session.state.live();
        let mapper = PathMapper::new(&self.project, &live);
        let result = crate::trusted::exec_reusing_bash(
            &mut session.shell,
            ExecRequest {
                argv: argv
                    .into_iter()
                    .map(|value| mapper.map_arg(&value))
                    .collect(),
                cwd: Some(
                    cwd.as_deref()
                        .map(|path| mapper.map_path(path))
                        .unwrap_or(live),
                ),
                env: env
                    .map(|values| {
                        values
                            .into_iter()
                            .map(|(key, value)| {
                                mapper.map_env(&key, &value).map(|value| (key, value))
                            })
                            .collect::<Result<Vec<_>>>()
                    })
                    .transpose()?,
            },
        );
        // The helper's boundary request performs FSEventStreamFlushSync. Since
        // the child has already exited, one post-command boundary captures all
        // of its mutations without creating a marker file or a second roundtrip.
        let tracked = self.prepare_events(&mut session.state, false).map(|_| ());
        let result = result?;
        tracked?;
        self.enforce_fast_storage_limit(&session)?;
        session.append(
            &session_id,
            JournalKind::Exec,
            Some(serde_json::json!({"label": label, "result": &result})),
        )?;
        Ok(result)
    }

    fn checkpoint_fast_locked(
        &self,
        session: &mut FastSession,
        params: CheckpointParams,
    ) -> Result<CheckpointInfo> {
        self.require_fast_running(session)?;
        let (id, changed) = session.state.checkpoint(params.label.clone())?;
        session.append(&params.session_id, JournalKind::Checkpoint, None)?;
        Ok(CheckpointInfo {
            id: id.to_string(),
            label: params.label,
            created_at_ms: now_ms(),
            change_count: changed,
            committed: false,
        })
    }

    fn diff_fast_locked(
        &self,
        session: &mut FastSession,
        params: DiffParams,
    ) -> Result<DiffResult> {
        self.require_fast_running(session)?;
        self.enforce_fast_storage_limit(session)?;
        let from = parse_checkpoint(params.from.as_deref())?;
        let to = parse_checkpoint(params.to.as_deref())?;
        let changes = session.state.diff(from, to)?;
        session.append(&params.session_id, JournalKind::Diff, None)?;
        Ok(DiffResult {
            from: params.from,
            to: params.to,
            pending_changes: changes.len(),
            changes,
        })
    }

    fn commit_fast_locked(
        &self,
        session: &mut FastSession,
        params: CommitParams,
    ) -> Result<CommitResult> {
        self.require_fast_running(session)?;
        self.enforce_fast_storage_limit(session)?;
        // Embedded sessions are serialized by the in-process FastSession
        // mutex rather than the per-session file lock; that is enough only
        // because a fast session is driven solely from the process that
        // created it. Publication is different: it lands in the host that
        // sibling sessions and other processes share, so the publish lock and
        // the publog bookkeeping are taken exactly as in `commit_locked`.
        let _publish =
            Lock::acquire(&crate::state_layout::publish_lock_path(&self.project)?)?;
        crate::test_hooks::maybe_pause("publish_lock_held");
        let target = parse_checkpoint(params.checkpoint.as_deref())?;
        let (seen, folded) = self.publog_cursor(&params.session_id)?;
        let endorsed_until = endorsement_bound(&params, seen);
        let (id, published) = session.state.commit(target, endorsed_until)?;
        let applied = published.len();
        self.record_publication(&params.session_id, &published)?;
        session.review = None;
        self.save_review(&params.session_id, None)?;
        session.append(&params.session_id, JournalKind::Commit, None)?;
        Ok(CommitResult {
            checkpoint_id: id.to_string(),
            applied,
            folded_publications: (folded > 0).then_some(folded),
        })
    }

    fn abort_fast_locked(
        &self,
        session: &mut FastSession,
        params: AbortParams,
    ) -> Result<AbortResult> {
        self.require_fast_running(session)?;
        let target = parse_checkpoint(params.checkpoint.as_deref())?;
        let (id, restored) = session.state.abort(target)?;
        session.append(&params.session_id, JournalKind::Abort, None)?;
        Ok(AbortResult {
            checkpoint_id: id.to_string(),
            restored,
        })
    }

    fn prepare_events(&self, state: &mut State, complete: bool) -> Result<EventTracker> {
        let status = state.status();
        let live = state.live();
        let uuid = device_uuid(&live)?;
        let since = if uuid == status.device_uuid {
            status.event_id
        } else {
            current_event_id()
        };
        fs::create_dir_all(live.join(".deltabox"))?;
        let strict = state.durable();
        if let Ok(mut batch) = self.helper_batch(state, since, strict || complete) {
            if uuid != status.device_uuid {
                batch.must_scan = true;
            }
            batch.latest_id = batch.latest_id.max(since);
            state.reconcile(&batch, strict, uuid)?;
            return Ok(EventTracker::Helper);
        }
        let stream = EventStream::start(std::slice::from_ref(&live), since)?;
        let mut batch = stream.history(EVENT_TIMEOUT)?;
        if strict || complete {
            batch.merge(stream.barrier(&live.join(".deltabox/native-barrier"), EVENT_TIMEOUT)?);
        } else {
            batch.merge(stream.flush());
        }
        if uuid != status.device_uuid {
            batch.must_scan = true;
        }
        batch.latest_id = batch.latest_id.max(since);
        state.reconcile(&batch, strict, uuid)?;
        Ok(EventTracker::Stream(stream))
    }

    fn finish_events(
        &self,
        state: &mut State,
        tracker: &EventTracker,
        reconcile: bool,
    ) -> Result<()> {
        let batch = match tracker {
            EventTracker::Helper => match event_helper_barrier(&state.event_socket()) {
                Ok(batch) => batch,
                Err(_) => {
                    let status = state.status();
                    let stream = EventStream::start(&[state.live()], status.event_id)?;
                    let mut batch = stream.history(EVENT_TIMEOUT)?;
                    batch.merge(stream.barrier(
                        &state.live().join(".deltabox/native-barrier"),
                        EVENT_TIMEOUT,
                    )?);
                    batch
                }
            },
            EventTracker::Stream(stream) => stream.barrier(
                &state.live().join(".deltabox/native-barrier"),
                EVENT_TIMEOUT,
            )?,
        };
        let uuid = device_uuid(&state.live())?;
        if reconcile {
            state.reconcile(&batch, false, uuid)?;
        } else {
            state.set_event_boundary(batch.latest_id, uuid)?;
        }
        Ok(())
    }

    fn reconcile_history(&self, state: &mut State) -> Result<()> {
        self.prepare_events(state, true)?;
        Ok(())
    }

    fn mutate_live(
        &self,
        state: &mut State,
        operation: impl FnOnce(&mut State) -> Result<()>,
    ) -> Result<()> {
        let stream = self.prepare_events(state, true)?;
        operation(state)?;
        self.finish_events(state, &stream, false)
    }

    fn helper_batch(&self, state: &State, since: u64, strict: bool) -> Result<EventBatch> {
        let socket = state.event_socket();
        let request = || {
            if strict {
                event_helper_barrier(&socket)
            } else {
                event_helper_boundary(&socket)
            }
        };
        if let Ok(batch) = request() {
            return Ok(batch);
        }
        let mut child = Command::new(&self.helper_bin)
            .arg("macos-events")
            .arg("--socket")
            .arg(&socket)
            .arg("--live")
            .arg(state.live())
            .arg("--since")
            .arg(since.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(batch) = request() {
                return Ok(batch);
            }
            if let Some(status) = child.try_wait()? {
                return Err(Error::Backend(format!(
                    "macOS event helper exited with {status}"
                )));
            }
            if Instant::now() >= deadline {
                return Err(Error::Backend("macOS event helper timed out".into()));
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn enforce_storage_limit(&self, meta: &SessionMeta, state: &State) -> Result<()> {
        let Some(limit) = self.policy(meta)?.limits.max_storage_bytes else {
            return Ok(());
        };
        let used = state
            .diff(None, None)?
            .iter()
            .filter_map(|change| change.new_size)
            .sum::<u64>();
        if used > limit {
            return Err(Error::Backend(format!(
                "pending changes use {used} bytes, exceeding max_storage_bytes"
            )));
        }
        Ok(())
    }

    fn open_state(&self, record: &Record) -> Result<State> {
        let engine = record.state_root.join("engine");
        // State::open rolls back a torn transaction when one is on disk; that
        // host mutation must not interleave with a sibling's publication.
        // The lock-free existence check keeps ordinary operations cheap; the
        // recovery re-checks the transaction file after the lock is held.
        let _publish = if engine.join("transaction.json").exists() {
            Some(Lock::acquire(&crate::state_layout::publish_lock_path(
                &self.project,
            )?)?)
        } else {
            None
        };
        State::open(&self.project, &engine)
    }

    fn policy(&self, meta: &SessionMeta) -> Result<Policy> {
        match &meta.policy_path {
            Some(path) => Policy::load(path),
            None => Ok(Policy::default_agent()),
        }
    }

    fn running_meta(&self, session_id: &str) -> Result<SessionMeta> {
        let meta = self.load_meta(session_id)?;
        if meta.status == SessionStatus::Running {
            Ok(meta)
        } else {
            Err(Error::SessionStopped(session_id.into()))
        }
    }

    fn load_meta(&self, session_id: &str) -> Result<SessionMeta> {
        let path = self.store(session_id).join(SESSION_META);
        if !path.exists() {
            return Err(Error::SessionNotFound(session_id.into()));
        }
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    }

    fn save_meta(&self, meta: &SessionMeta) -> Result<()> {
        let durable = meta.filesystem.durability == "strict";
        save_json(
            &self.store(&meta.session_id).join(SESSION_META),
            meta,
            durable,
        )?;
        SessionRegistry::update(&self.root, |registry| registry.upsert(meta.clone()))
    }

    fn record(&self, session_id: &str) -> Result<Record> {
        Ok(serde_json::from_slice(&fs::read(
            self.store(session_id).join(RECORD_FILE),
        )?)?)
    }

    fn review(&self, session_id: &str) -> Result<Option<ActiveReview>> {
        let path = self.store(session_id).join(ACTIVE_REVIEW);
        if path.exists() {
            Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
        } else {
            Ok(None)
        }
    }

    fn require_review(&self, session_id: &str) -> Result<ActiveReview> {
        self.review(session_id)?
            .ok_or_else(|| Error::ReviewNotActive(session_id.into()))
    }

    fn save_review(&self, session_id: &str, review: Option<&ActiveReview>) -> Result<()> {
        let path = self.store(session_id).join(ACTIVE_REVIEW);
        match review {
            Some(review) => save_json(&path, review, false),
            None if path.exists() => fs::remove_file(path).map_err(Error::from),
            None => Ok(()),
        }
    }

    fn append(
        &self,
        record: &Record,
        session_id: &str,
        kind: JournalKind,
        data: Option<serde_json::Value>,
    ) -> Result<()> {
        let mut journal = Journal::open(&self.store(session_id).join("journal.jsonl"))?;
        append_journal(&mut journal, record, session_id, kind, data)
    }

    fn store(&self, session_id: &str) -> PathBuf {
        self.root.join(session_id)
    }

    fn lock(&self, session_id: &str) -> Result<SessionOperationLock> {
        let session = self.exec_lock(session_id)?;
        if !self.store(session_id).join(SESSION_META).exists() {
            return Err(Error::SessionNotFound(session_id.into()));
        }
        let record = self.record(session_id)?;
        let operation = command_executor::lock_operations(&command_endpoint(&record))?;
        Ok(SessionOperationLock {
            _operation: operation,
            _session: session,
        })
    }

    fn exec_lock(&self, session_id: &str) -> Result<Lock> {
        Lock::acquire(&self.store(session_id).join(SESSION_LOCK))
    }
}

impl FastSession {
    fn append(
        &mut self,
        session_id: &str,
        kind: JournalKind,
        data: Option<serde_json::Value>,
    ) -> Result<()> {
        append_journal(&mut self.journal, &self.record, session_id, kind, data)
    }
}

fn append_journal(
    journal: &mut Journal,
    record: &Record,
    session_id: &str,
    kind: JournalKind,
    data: Option<serde_json::Value>,
) -> Result<()> {
    journal.append(JournalEntry {
        ts_ms: now_ms(),
        session_id: session_id.into(),
        kind,
        data,
    })?;
    if record.durability == DurabilityMode::Strict {
        journal.flush_sync()?;
    }
    Ok(())
}

struct SessionOperationLock {
    _operation: command_executor::OperationLock,
    _session: Lock,
}

fn resolve_policy(project: &Path, policy: Option<&Path>) -> Result<Option<PathBuf>> {
    policy
        .map(|path| {
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                project.join(path)
            }
            .canonicalize()
            .map_err(Error::from)
        })
        .transpose()
}

fn state_base(project: &Path) -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("DELTABOX_STATE_DIR") {
        let base = PathBuf::from(path);
        // Cloning the project into a state dir inside the project recurses
        // until the path length limit. Refuse it up front.
        if base.starts_with(project) {
            return Err(Error::Backend(format!(
                "state directory {} must live outside the project {}",
                base.display(),
                project.display()
            )));
        }
        return Ok(base.join("macos-native"));
    }
    let parent = project
        .parent()
        .ok_or_else(|| Error::Backend("project has no parent directory".into()))?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    project.hash(&mut hasher);
    Ok(parent
        .join(".deltabox-lite-state")
        .join(format!("{:016x}", hasher.finish())))
}

fn probe_clonefile(project_state: &Path, state_root: &Path, session_id: &str) -> Result<()> {
    // Concurrent unlocked starts share the probe scratch directory, so the
    // source name must be per-session or create/remove would race.
    let source = project_state.join(format!(".apfs-clone-probe-{session_id}"));
    let destination = state_root.join(".apfs-clone-probe");
    fs::write(&source, b"deltafs")?;
    let source_c = CString::new(source.as_os_str().as_bytes()).expect("probe path");
    let destination_c = CString::new(destination.as_os_str().as_bytes()).expect("probe path");
    let result = unsafe { libc::clonefile(source_c.as_ptr(), destination_c.as_ptr(), 0) };
    let error = std::io::Error::last_os_error();
    fs::remove_file(source)?;
    if result != 0 {
        return Err(Error::FilesystemUnavailable(format!(
            "APFS clonefile is unavailable between project and state directory: {error}"
        )));
    }
    fs::remove_file(destination)?;
    Ok(())
}

fn mode_for_lease(mode: Mode) -> crate::protocol::FilesystemMode {
    match mode {
        Mode::Through => crate::protocol::FilesystemMode::CbcowThrough,
        Mode::Private => crate::protocol::FilesystemMode::CbcowPrivate,
    }
}

fn filesystem_info(mode: Mode, state: &State, durability: DurabilityMode) -> FilesystemInfo {
    let mut info = FilesystemInfo::macos_native(match mode {
        Mode::Through => FilesystemMode::CbcowThrough,
        Mode::Private => FilesystemMode::CbcowPrivate,
    });
    info.tainted = state.status().tainted;
    info.mountpoint = Some(state.live());
    info.with_durability(durability, None)
}

fn command_endpoint(record: &Record) -> CommandEndpoint {
    // Darwin's sockaddr_un.sun_path is only 104 bytes. Keep control artifacts
    // in a short, per-user/per-session directory even when the project or an
    // overridden DELTABOX_STATE_DIR has a long path. The authentication token
    // remains in the private durable state root.
    let session_id = record
        .state_root
        .file_name()
        .expect("macOS state root has a session directory");
    let runtime = Path::new("/private/tmp")
        .join(format!("deltabox-{}", unsafe { libc::geteuid() }))
        .join(session_id);
    CommandEndpoint::new(
        runtime.join("command.sock"),
        record.state_root.join("command-token"),
        runtime.join("command.ready"),
    )
}

fn info_from_stopped(meta: SessionMeta) -> SessionInfo {
    SessionInfo {
        session_id: meta.session_id,
        project: meta.project,
        started_at_ms: meta.started_at_ms,
        status: meta.status,
        checkpoint_count: 0,
        pending_changes: 0,
        filesystem: meta.filesystem,
    }
}

/// The endorsement bound is normally the session's own publog cursor: only
/// publications it has acknowledged may move the baseline, so
/// first-committer-wins holds. The pre-declared `ours` policy waives that
/// acknowledgement inside the same lock window — every digest-endorsed sibling
/// publication is accepted and overridden, while stranger changes keep
/// failing closed.
fn endorsement_bound(params: &CommitParams, seen: u64) -> u64 {
    if params.ours {
        u64::MAX
    } else {
        seen
    }
}

fn parse_checkpoint(value: Option<&str>) -> Result<Option<u64>> {
    value
        .map(|value| {
            value
                .parse()
                .map_err(|_| Error::CheckpointNotFound(value.into()))
        })
        .transpose()
}

fn relative_path(project: &Path, path: &Path) -> Result<PathBuf> {
    let relative = if path.is_absolute() {
        path.strip_prefix(project)
            .map_err(|_| Error::Backend("path is outside project".into()))?
    } else {
        path
    };
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) || relative
        .components()
        .next()
        .is_some_and(|component| component.as_os_str() == ".deltabox")
    {
        return Err(Error::Backend("path is outside project".into()));
    }
    Ok(relative.to_path_buf())
}

fn checked_path(root: &Path, relative: &Path) -> Result<PathBuf> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(Error::Backend("path crosses a symlink".into()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(root.join(relative))
}

fn save_json(path: &Path, value: &impl Serialize, durable: bool) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    let mut file = File::create(&temporary)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    if durable {
        file.sync_all()?;
    }
    fs::rename(temporary, path)?;
    if durable {
        File::open(path.parent().expect("saved path has parent"))?.sync_all()?;
    }
    Ok(())
}

fn write_file(path: &Path, bytes: &[u8], durable: bool) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    if durable {
        file.sync_all()?;
        File::open(path.parent().expect("written path has parent"))?.sync_all()?;
    }
    Ok(())
}

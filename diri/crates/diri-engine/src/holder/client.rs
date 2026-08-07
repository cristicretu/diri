//! Daemon-side clients: one for a session holder, one for the shared manager.
//!
//! One connection per request, by design — the protocol is a single
//! request/response line, and connectionless clients survive the holder
//! outliving any number of daemons.

use std::path::{Path, PathBuf};

use base64::Engine as _;

use super::protocol::{
    HolderLaunchSpec, HolderManagerRequest, HolderManagerResponse, HolderOperation,
    HolderProcessSample, HolderRequest, HolderResponse, HolderStat,
};
use super::socket;
use super::{HolderError, HolderResult};

/// Control client for one session holder.
#[derive(Clone, Debug)]
pub struct HolderClient {
    pub socket_path: PathBuf,
}

impl HolderClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    /// Sends bytes to the held child, as if typed.
    pub fn write(&self, data: &[u8]) -> HolderResult<()> {
        let mut request = HolderRequest::op(HolderOperation::Write);
        request.data = Some(base64::engine::general_purpose::STANDARD.encode(data));
        self.request(&request).map(drop)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> HolderResult<()> {
        let mut request = HolderRequest::op(HolderOperation::Resize);
        request.cols = Some(cols);
        request.rows = Some(rows);
        self.request(&request).map(drop)
    }

    /// Signals the whole child tree; returns the processes that were signalled.
    pub fn signal(&self, sig: i32) -> HolderResult<Vec<HolderProcessSample>> {
        let mut request = HolderRequest::op(HolderOperation::Signal);
        request.sig = Some(sig);
        Ok(self.request(&request)?.tree.unwrap_or_default())
    }

    pub fn kill_tree(&self) -> HolderResult<()> {
        self.request(&HolderRequest::op(HolderOperation::KillTree))
            .map(drop)
    }

    pub fn stat(&self) -> HolderResult<HolderStat> {
        self.request(&HolderRequest::op(HolderOperation::Stat))?
            .stat
            .ok_or_else(|| HolderError::Transport("stat response omitted stat".into()))
    }

    /// Whether a live holder with a live child serves this socket.
    pub fn is_alive(&self) -> bool {
        self.stat().map(|stat| stat.alive).unwrap_or(false)
    }

    fn request(&self, request: &HolderRequest) -> HolderResult<HolderResponse> {
        let mut stream = socket::connect(&self.socket_path)?;
        socket::write_json_line(&mut stream, request)?;
        let response: HolderResponse = socket::read_json_line(&mut stream)?;
        if !response.ok {
            return Err(HolderError::Rejected(
                response.error.unwrap_or_else(|| "unknown error".into()),
            ));
        }
        Ok(response)
    }
}

/// Control client for the one lightweight holder manager in a registry.
///
/// Session traffic never flows through this socket. It is used only to ask
/// the manager to create a session-local holder; per-session sockets, logs,
/// and restart adoption semantics are unchanged by its existence.
#[derive(Clone, Debug)]
pub struct HolderManagerClient {
    pub socket_path: PathBuf,
}

impl HolderManagerClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    pub fn ping(&self) -> HolderResult<i32> {
        self.request(&HolderManagerRequest::ping())
    }

    /// Asks the manager to host a holder for `spec`; returns the manager pid.
    pub fn launch(&self, spec: &HolderLaunchSpec) -> HolderResult<i32> {
        self.request(&HolderManagerRequest::launch(spec.clone()))
    }

    pub fn is_alive(&self) -> bool {
        self.ping().is_ok()
    }

    fn request(&self, request: &HolderManagerRequest) -> HolderResult<i32> {
        let mut stream = socket::connect(&self.socket_path)?;
        socket::write_json_line(&mut stream, request)?;
        let response: HolderManagerResponse = socket::read_json_line(&mut stream)?;
        if !response.ok {
            return Err(HolderError::Rejected(
                response
                    .error
                    .unwrap_or_else(|| "unknown manager error".into()),
            ));
        }
        match response.manager_pid {
            Some(pid) if pid > 1 => Ok(pid),
            _ => Err(HolderError::Transport(
                "manager response omitted pid".into(),
            )),
        }
    }
}

impl HolderClient {
    /// Convenience over `Path` without an allocation at every call site.
    pub fn at(path: &Path) -> Self {
        Self::new(path.to_path_buf())
    }
}

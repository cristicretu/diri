//! Pure path construction and shared environment names.
//!
//! This crate deliberately does not discover a home directory or touch the
//! filesystem. Callers provide the user's home directory.

use std::path::{Path, PathBuf};

pub const APP_SUPPORT_RELATIVE_PATH: &str = "Library/Application Support/Dirijor";
pub const SOCKET_FILE_NAME: &str = "daemon.sock";
pub const STATE_FILE_NAME: &str = "state.json";
pub const LOGS_DIR_NAME: &str = "logs";
pub const INJECT_DIR_NAME: &str = "inject";
pub const BIN_DIR_NAME: &str = "bin";
pub const MANIFEST_OVERRIDES_RELATIVE_PATH: &str = "manifests/overrides";
pub const DAEMON_LOG_FILE_NAME: &str = "dirijord.log";
pub const HOSTS_CONFIG_FILE_NAME: &str = "hosts.json";

pub const ENV_SESSION_ID: &str = "DIRIJOR_SESSION_ID";
pub const ENV_SOCKET: &str = "DIRIJOR_SOCKET";
pub const ENV_CLI: &str = "DIRIJOR_CLI";

pub struct DirijorPaths;

impl DirijorPaths {
    pub fn app_support(home: impl AsRef<Path>) -> PathBuf {
        home.as_ref().join(APP_SUPPORT_RELATIVE_PATH)
    }

    pub fn socket(home: impl AsRef<Path>) -> PathBuf {
        Self::app_support(home).join(SOCKET_FILE_NAME)
    }

    pub fn state_file(home: impl AsRef<Path>) -> PathBuf {
        Self::app_support(home).join(STATE_FILE_NAME)
    }

    pub fn logs_dir(home: impl AsRef<Path>) -> PathBuf {
        Self::app_support(home).join(LOGS_DIR_NAME)
    }

    pub fn inject_dir(home: impl AsRef<Path>) -> PathBuf {
        Self::app_support(home).join(INJECT_DIR_NAME)
    }

    pub fn bin_dir(home: impl AsRef<Path>) -> PathBuf {
        Self::app_support(home).join(BIN_DIR_NAME)
    }

    pub fn manifest_overrides_dir(home: impl AsRef<Path>) -> PathBuf {
        Self::app_support(home).join(MANIFEST_OVERRIDES_RELATIVE_PATH)
    }

    pub fn daemon_log_file(home: impl AsRef<Path>) -> PathBuf {
        Self::app_support(home).join(DAEMON_LOG_FILE_NAME)
    }

    pub fn hosts_config_file(home: impl AsRef<Path>) -> PathBuf {
        Self::app_support(home).join(HOSTS_CONFIG_FILE_NAME)
    }
}

pub struct DirijorEnv;

impl DirijorEnv {
    pub const SESSION_ID: &'static str = ENV_SESSION_ID;
    pub const SOCKET: &'static str = ENV_SOCKET;
    pub const CLI: &'static str = ENV_CLI;
}

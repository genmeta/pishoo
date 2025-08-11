use std::fs;

use anyhow::{Context, Result};
use nix::{
    sys::signal::{self, Signal},
    unistd::Pid,
};

pub fn send_signal(pid_file: &str, signal_type: &str) -> Result<()> {
    Err(anyhow::anyhow!(
        "the -s option is only supported on Linux and macOS"
    ))
}

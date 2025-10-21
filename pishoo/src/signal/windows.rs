use gateway::error::Whatever;
use snafu::whatever;

pub fn send_signal(_pid_file: &str, _signal_type: &str) -> Result<(), Whatever> {
    whatever!("Signal handling is only supported on Unix-like systems.")
}

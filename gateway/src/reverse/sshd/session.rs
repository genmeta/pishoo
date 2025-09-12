use std::os::unix::prelude::{AsFd, AsRawFd, CommandExt};

use futures::{SinkExt, TryStreamExt};
use nix::{
    libc, pty,
    sys::{self, socket},
    unistd,
};
use snafu::ResultExt;
use tokio::io::{self, AsyncWriteExt};
use tokio_util::{io::ReaderStream, task::AbortOnDropHandle};

use super::{
    async_fd::{AsyncFd, OwnedReadHalf, OwnedWriteHalf},
    messages::session::{ClientSessionMessage, ServerSessionMessage},
    mux::{FramedRecver, FramedSender, Recver, Sender},
};
use crate::error::Whatever;

pub async fn shell(
    user: &unistd::User,
    pseudo: bool,
    recver: Recver,
    sender: Sender,
) -> Result<impl Future<Output = ()> + use<>, Whatever> {
    exec(user, pseudo, None, recver, sender).await
}

pub async fn exec(
    user: &unistd::User,
    pseudo: bool,
    command: Option<&str>,
    recver: Recver,
    sender: Sender,
) -> Result<impl Future<Output = ()> + use<>, Whatever> {
    // TOOD: do_exec_no_pty
    let (child, child_io) = match pseudo {
        true => do_exec_pty(user, command).whatever_context(format!(
            "Failed to exec `{}` pty",
            command.unwrap_or("<no command>")
        ))?,
        false => do_exec_no_pty(user, command).whatever_context(format!(
            "Failed to exec `{}` without pty",
            command.unwrap_or("<no command>")
        ))?,
    };
    tracing::info!(target: "session", "Child process {child} started");
    let (mut child_read_half, mut child_write_half) = child_io.split();

    let mut sender = sender.framed();
    let mut close_sender = sender.clone();
    let mut recver = recver.framed::<ClientSessionMessage>();

    Ok(async move {
        let _send_terminal = AbortOnDropHandle::new(tokio::spawn(async move {
            send_terminal(&mut child_read_half, &mut sender).await
        }));
        let recv_terminal = AbortOnDropHandle::new(tokio::spawn(async move {
            recv_terminal(&mut child_write_half, &mut recver).await
        }));

        let child_exit =
            AbortOnDropHandle::new(tokio::task::spawn_blocking(move || wait_child_exit(child)));

        // TOOD: status code
        tokio::select! {
            _ = recv_terminal => {
                tracing::debug!(target: "session", "Terminal receiver finished unexpectedly");
                // If terminal receiver finished unexpectedly, we need to kill the child process
                let _ = sys::signal::kill(child, sys::signal::Signal::SIGHUP)
                    .map_err(|e| tracing::warn!(target: "session", "Failed to send SIGHUP to child when client disconnected: {e}"));
            },
            code = child_exit => match code.unwrap_or_else(|e| Err(io::Error::other(e))) {
                Ok(code) => {
                    close_sender
                        .send(ServerSessionMessage::Exit { code })
                        .await
                        .unwrap_or_else(|e| tracing::error!(target: "session", "Failed to send exit code: {e}"));
                }
                Err(e) => {
                    tracing::error!(target: "session", "Failed to wait child process exit: {e}");
                    close_sender
                        .cancel(io::Error::other("Server internal error"))
                        .await
                        .unwrap_or_else(|e| tracing::error!(target: "session", "Failed to cancel sender: {e}"));
                }
            }
        };
    })
}

async fn send_terminal(
    pty_read_half: &mut OwnedReadHalf,
    message_sender: &mut FramedSender<ServerSessionMessage>,
) -> io::Result<()> {
    let mut pty_read_stream = ReaderStream::new(pty_read_half);
    while let Some(sequence) = pty_read_stream.try_next().await? {
        message_sender
            .send(ServerSessionMessage::Sequence(sequence))
            .await?;
    }
    Ok(())
}

async fn recv_terminal(
    pty_write_half: &mut OwnedWriteHalf,
    recver: &mut FramedRecver<ClientSessionMessage>,
) -> io::Result<()> {
    while let Some(terminal_message) = recver.try_next().await? {
        match terminal_message {
            ClientSessionMessage::WindowSize { rows, cols } => {
                // 设置PTY窗口大小
                unsafe {
                    let winsz = libc::winsize {
                        ws_row: rows,
                        ws_col: cols,
                        ws_xpixel: 0,
                        ws_ypixel: 0,
                    };
                    libc::ioctl(pty_write_half.as_fd().as_raw_fd(), libc::TIOCSWINSZ, &winsz);
                }
            }
            ClientSessionMessage::Sequence(sequence) => {
                // 发送数据到shell
                if let Err(e) = pty_write_half.write_all(&sequence).await {
                    tracing::debug!(target: "session", "Failed to write sequence to PTY: {e}");
                    break;
                }
            }
        }
    }
    Ok(())
}

fn wait_child_exit(child: unistd::Pid) -> io::Result<i32> {
    loop {
        match sys::wait::waitpid(child, None)? {
            sys::wait::WaitStatus::Exited(pid, code) => {
                tracing::info!(target: "session", "Child process {pid} exited with code {code}");
                return Ok(code);
            }
            sys::wait::WaitStatus::Signaled(pid, signal, coredump) => {
                tracing::info!(target: "session", coredump, "Child process {pid} was killed by signal {signal:?}");
                return Ok(128 + signal as i32);
            }
            _ => continue,
        }
    }
}

fn do_exec_pty(
    user: &unistd::User,
    command: Option<&str>,
) -> Result<(unistd::Pid, AsyncFd), Whatever> {
    // 创建一个伪终端，frok子进程，设置终端为登录终端
    match unsafe { pty::forkpty(None, None) }.whatever_context("Failed to fork pty")? {
        pty::ForkptyResult::Parent { child, master } => Ok((
            child,
            AsyncFd::new(master).whatever_context("Failed to create async file descriptor")?,
        )),
        pty::ForkptyResult::Child => do_child(user, command),
    }
}

// 在 terminal.rs 中添加
fn do_exec_no_pty(
    user: &unistd::User,
    command: Option<&str>,
) -> Result<(unistd::Pid, AsyncFd), Whatever> {
    let (parent_sock, child_sock) = socket::socketpair(
        socket::AddressFamily::Unix,
        socket::SockType::Stream,
        None,
        socket::SockFlag::empty(),
    )
    .whatever_context("Failed to create socket pair for terminal IO IPC")?;

    match unsafe { unistd::fork() }.whatever_context("Failed to fork subprocess")? {
        unistd::ForkResult::Parent { child } => {
            // 父进程关闭读端，将写端转换为AsyncFd
            drop(child_sock);
            Ok((
                child,
                AsyncFd::new(parent_sock)
                    .whatever_context("Failed to create async file descriptor")?,
            ))
        }
        unistd::ForkResult::Child => {
            drop(parent_sock);
            // 子进程关闭写端，将读端设为标准输入

            // 将管道读端设为标准输入
            if let Err(e) = unistd::dup2_stdin(child_sock.as_fd()) {
                eprintln!("Failed to dup2 stdin: {e}");
                std::process::exit(1);
            }
            if let Err(e) = unistd::dup2_stdout(child_sock.as_fd()) {
                eprintln!("Failed to dup2 stdout: {e}");
                std::process::exit(1);
            }
            if let Err(e) = unistd::dup2_stderr(child_sock.as_fd()) {
                eprintln!("Failed to dup2 stderr: {e}");
                std::process::exit(1);
            }

            // 执行命令
            do_child(user, command)
        }
    }
}

fn do_child(user: &unistd::User, command: Option<&str>) -> ! {
    let shell0 = user
        .shell
        .file_name()
        .expect("path terminates wont be `..`.");

    // 设置环境变量，设置uid，gid，调用exec等操作由标准库完成
    // 这里不会返回，除非exec失败
    let mut call = std::process::Command::new(&user.shell);
    call.gid(user.gid.as_raw())
        .uid(user.uid.as_raw())
        .current_dir(&user.dir)
        .env("HOME", &user.dir)
        .env("USER", &user.name)
        .env("SHELL", &user.shell)
        .env("TERM", "xterm-256color")
        .arg0(shell0);
    if let Some(command) = command {
        call.args(["-c", command]);
    }

    let exec_error = call.exec();

    eprintln!("Server internal error(Failed to exec shell: {exec_error:?})");
    std::process::exit(1)
}

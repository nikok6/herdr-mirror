use std::io;
use std::time::Duration;

use tokio::process::Child;
use tokio::sync::{oneshot, watch};

const TERM_GRACE: Duration = Duration::from_millis(250);
const KILL_GRACE: Duration = Duration::from_millis(200);

pub(crate) struct ChildSupervisor {
    cancel: Option<oneshot::Sender<()>>,
    exited: watch::Receiver<bool>,
}

impl ChildSupervisor {
    pub(crate) fn start(mut child: Child) -> (Self, watch::Receiver<bool>) {
        let pid = child.id().map(|value| value as i32);
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let (exited_tx, exited_rx) = watch::channel(false);
        tokio::spawn(async move {
            let reaped = tokio::select! {
                status = child.wait() => status.is_ok(),
                _ = cancel_rx => terminate_and_reap(&mut child, pid).await,
            };
            if reaped {
                let _ = exited_tx.send(true);
            }
        });
        (
            Self {
                cancel: Some(cancel_tx),
                exited: exited_rx.clone(),
            },
            exited_rx,
        )
    }

    pub(crate) async fn cancel_and_wait(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        if *self.exited.borrow() {
            return;
        }
        let _ = tokio::time::timeout(
            Duration::from_millis(500),
            self.exited.wait_for(|exited| *exited),
        )
        .await;
    }
}

impl Drop for ChildSupervisor {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

async fn terminate_and_reap(child: &mut Child, pid: Option<i32>) -> bool {
    if let Some(pid) = pid {
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
    if matches!(
        tokio::time::timeout(TERM_GRACE, child.wait()).await,
        Ok(Ok(_))
    ) {
        return true;
    }
    let _ = child.start_kill();
    matches!(
        tokio::time::timeout(KILL_GRACE, child.wait()).await,
        Ok(Ok(_))
    )
}

pub(crate) async fn wait_for_exit(exited: &mut watch::Receiver<bool>) -> io::Result<()> {
    if *exited.borrow() {
        return Ok(());
    }
    exited
        .wait_for(|value| *value)
        .await
        .map(|_| ())
        .map_err(|_| io::Error::other("child supervisor stopped before reporting exit"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::Instant;

    #[tokio::test]
    async fn cancellation_reaps_a_term_ignoring_child_within_the_bound() {
        let mut command = tokio::process::Command::new("sh");
        command
            .args(["-c", "trap '' TERM; while :; do sleep 1; done"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().unwrap();
        let pid = child.id().unwrap() as i32;
        let (mut supervisor, _) = ChildSupervisor::start(child);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let started = Instant::now();
        supervisor.cancel_and_wait().await;
        assert!(started.elapsed() < Duration::from_millis(550));
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "child must be reaped");
    }
}

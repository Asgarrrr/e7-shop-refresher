//! Console input: stdin lines in, [`Command`]s out. Knows nothing about
//! capture, reassembly or session state.

use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, watch};

use crate::journal::EventLog;

use super::Command;

/// The session loop never touches stdin: it only ever sees [`Command`]s.
pub(super) async fn stdin_loop(
    commands: mpsc::Sender<Command>,
    shutdown: watch::Receiver<bool>,
    journal: EventLog,
) {
    input_loop(
        BufReader::new(tokio::io::stdin()),
        commands,
        shutdown,
        &journal,
    )
    .await;
}

/// Injectable in tests so a pending read can be cancelled without touching
/// process stdin. Unknown input goes through `journal`, not `println!`: stdout
/// is inert in the windowed build, where the log file still is not.
async fn input_loop(
    input: impl AsyncBufRead + Unpin,
    commands: mpsc::Sender<Command>,
    mut shutdown: watch::Receiver<bool>,
    journal: &EventLog,
) {
    let mut lines = input.lines();
    loop {
        if *shutdown.borrow() {
            break;
        }
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            line = lines.next_line() => match line {
                Ok(Some(line)) => match line.parse::<Command>() {
                    Ok(command) => {
                        if commands.send(command).await.is_err() {
                            break; // session loop gone.
                        }
                    }
                    // Wording belongs to `ParseCommandError`, next to the alias
                    // table it lists.
                    Err(err) => journal.emit(&[format!(">> {err}")]),
                },
                Ok(None) | Err(_) => break,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn worker_shutdown_pending_stdin_read_exits_on_signal() {
        let (reader, _writer) = tokio::io::duplex(64);
        let (commands, mut command_rx) = mpsc::channel(1);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let journal = EventLog::default();
        let task = tokio::spawn(async move {
            input_loop(BufReader::new(reader), commands, shutdown_rx, &journal).await;
        });
        tokio::task::yield_now().await;

        shutdown_tx.send_replace(true);
        task.await.unwrap();

        assert!(matches!(
            command_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }
}

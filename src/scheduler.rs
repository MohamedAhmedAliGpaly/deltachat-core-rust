use async_std::prelude::*;
use async_std::sync::{channel, Receiver, Sender};
use async_std::task;

use std::time::Duration;

use crate::context::Context;
use crate::error::Error;
use crate::imap::Imap;
use crate::job::{self, Thread};
use crate::smtp::Smtp;

/// Job and connection scheduler.
#[derive(Debug)]
pub(crate) enum Scheduler {
    Stopped,
    Running {
        inbox: ImapConnectionState,
        mvbox: ImapConnectionState,
        sentbox: ImapConnectionState,
        smtp: SmtpConnectionState,
    },
}

impl Context {
    pub(crate) async fn interrupt_inbox(&self) {
        self.scheduler.read().await.interrupt_inbox().await;
    }

    pub(crate) async fn interrupt_sentbox(&self) {
        self.scheduler.read().await.interrupt_sentbox().await;
    }

    pub(crate) async fn interrupt_mvbox(&self) {
        self.scheduler.read().await.interrupt_mvbox().await;
    }

    pub(crate) async fn interrupt_smtp(&self) {
        self.scheduler.read().await.interrupt_smtp().await;
    }
}

async fn inbox_loop(ctx: Context, inbox_handlers: ImapConnectionHandlers) {
    info!(ctx, "starting inbox loop");
    let ImapConnectionHandlers {
        mut connection,
        stop_receiver,
        shutdown_sender,
    } = inbox_handlers;

    let fut = async move {
        connection.connect_configured(&ctx).await.unwrap();

        loop {
            // TODO: correct value
            let probe_network = false;
            match job::load_next(&ctx, Thread::Imap, probe_network)
                .timeout(Duration::from_millis(200))
                .await
            {
                Ok(Some(job)) => {
                    job::perform_job(&ctx, job::Connection::Inbox(&mut connection), job).await;
                }
                Ok(None) | Err(async_std::future::TimeoutError { .. }) => {
                    let watch_folder = get_watch_folder(&ctx, "configured_inbox_folder")
                        .await
                        .ok_or_else(|| Error::WatchFolderNotFound("not-set".to_string()))
                        .unwrap();

                    // fetch
                    connection
                        .fetch(&ctx, &watch_folder)
                        .await
                        .unwrap_or_else(|err| {
                            error!(ctx, "{}", err);
                        });

                    // idle
                    if connection.can_idle() {
                        connection
                            .idle(&ctx, Some(watch_folder))
                            .await
                            .unwrap_or_else(|err| {
                                error!(ctx, "{}", err);
                            });
                    } else {
                        connection.fake_idle(&ctx, Some(watch_folder)).await;
                    }
                }
            }
        }
    };

    fut.race(stop_receiver.recv()).await;
    shutdown_sender.send(()).await;
}

async fn smtp_loop(ctx: Context, smtp_handlers: SmtpConnectionHandlers) {
    info!(ctx, "starting smtp loop");
    let SmtpConnectionHandlers {
        mut connection,
        stop_receiver,
        shutdown_sender,
        idle_interrupt_receiver,
    } = smtp_handlers;

    let fut = async move {
        loop {
            // TODO: correct value
            let probe_network = false;
            match job::load_next(&ctx, Thread::Smtp, probe_network)
                .timeout(Duration::from_millis(200))
                .await
            {
                Ok(Some(job)) => {
                    job::perform_job(&ctx, job::Connection::Smtp(&mut connection), job).await;
                }
                Ok(None) | Err(async_std::future::TimeoutError { .. }) => {
                    use futures::future::FutureExt;

                    // Fake Idle
                    async_std::task::sleep(Duration::from_millis(500))
                        .race(idle_interrupt_receiver.recv().map(|_| ()))
                        .await;
                }
            }
        }
    };

    fut.race(stop_receiver.recv()).await;
    shutdown_sender.send(()).await;
}

impl Scheduler {
    /// Start the scheduler, panics if it is already running.
    pub fn run(&mut self, ctx: Context) {
        match self {
            Scheduler::Stopped => {
                let (mvbox, mvbox_handlers) = ImapConnectionState::new();
                let (sentbox, sentbox_handlers) = ImapConnectionState::new();
                let (smtp, smtp_handlers) = SmtpConnectionState::new();
                let (inbox, inbox_handlers) = ImapConnectionState::new();

                let ctx1 = ctx.clone();
                let _ = task::spawn(async move { inbox_loop(ctx1, inbox_handlers).await });

                // TODO: mvbox
                // TODO: sentbox

                let ctx1 = ctx.clone();
                let _ = task::spawn(async move { smtp_loop(ctx1, smtp_handlers).await });

                *self = Scheduler::Running {
                    inbox,
                    mvbox,
                    sentbox,
                    smtp,
                };

                info!(ctx, "scheduler is running");
                println!("RUN DONE");
            }
            Scheduler::Running { .. } => {
                // TODO: return an error
                panic!("WARN: already running");
            }
        }
    }

    async fn interrupt_inbox(&self) {
        match self {
            Scheduler::Running { ref inbox, .. } => inbox.interrupt().await,
            _ => {}
        }
    }

    async fn interrupt_mvbox(&self) {
        match self {
            Scheduler::Running { ref mvbox, .. } => mvbox.interrupt().await,
            _ => {}
        }
    }

    async fn interrupt_sentbox(&self) {
        match self {
            Scheduler::Running { ref sentbox, .. } => sentbox.interrupt().await,
            _ => {}
        }
    }

    async fn interrupt_smtp(&self) {
        match self {
            Scheduler::Running { ref smtp, .. } => smtp.interrupt().await,
            _ => {}
        }
    }

    /// Halt the scheduler, panics if it is already stopped.
    pub async fn stop(&mut self) {
        match self {
            Scheduler::Stopped => {
                panic!("WARN: already stopped");
            }
            Scheduler::Running {
                inbox,
                mvbox,
                sentbox,
                smtp,
            } => {
                inbox
                    .stop()
                    .join(mvbox.stop())
                    .join(sentbox.stop())
                    .join(smtp.stop())
                    .await;
            }
        }
    }

    /// Check if the scheduler is running.
    pub fn is_running(&self) -> bool {
        match self {
            Scheduler::Running { .. } => true,
            _ => false,
        }
    }
}

/// Connection state logic shared between imap and smtp connections.
#[derive(Debug)]
struct ConnectionState {
    /// Channel to notify that shutdown has completed.
    shutdown_receiver: Receiver<()>,
    /// Channel to interrupt the whole connection.
    stop_sender: Sender<()>,
    /// Channel to interrupt idle.
    idle_interrupt_sender: Sender<()>,
}

impl ConnectionState {
    /// Shutdown this connection completely.
    async fn stop(&self) {
        // Trigger shutdown of the run loop.
        self.stop_sender.send(()).await;
        // Wait for a notification that the run loop has been shutdown.
        self.shutdown_receiver.recv().await;
    }

    async fn interrupt(&self) {
        self.idle_interrupt_sender.send(()).await;
    }
}

#[derive(Debug)]
pub(crate) struct SmtpConnectionState {
    state: ConnectionState,
}

impl SmtpConnectionState {
    fn new() -> (Self, SmtpConnectionHandlers) {
        let (stop_sender, stop_receiver) = channel(1);
        let (shutdown_sender, shutdown_receiver) = channel(1);
        let (idle_interrupt_sender, idle_interrupt_receiver) = channel(1);

        let handlers = SmtpConnectionHandlers {
            connection: Smtp::new(),
            stop_receiver,
            shutdown_sender,
            idle_interrupt_receiver,
        };

        let state = ConnectionState {
            idle_interrupt_sender,
            shutdown_receiver,
            stop_sender,
        };

        let conn = SmtpConnectionState { state };

        (conn, handlers)
    }

    /// Interrupt any form of idle.
    async fn interrupt(&self) {
        self.state.interrupt().await;
    }

    /// Shutdown this connection completely.
    async fn stop(&self) {
        self.state.stop().await;
    }
}

#[derive(Debug)]
struct SmtpConnectionHandlers {
    connection: Smtp,
    stop_receiver: Receiver<()>,
    shutdown_sender: Sender<()>,
    idle_interrupt_receiver: Receiver<()>,
}

#[derive(Debug)]
pub(crate) struct ImapConnectionState {
    state: ConnectionState,
}

impl ImapConnectionState {
    /// Construct a new connection.
    fn new() -> (Self, ImapConnectionHandlers) {
        let (stop_sender, stop_receiver) = channel(1);
        let (idle_interrupt_sender, idle_interrupt_receiver) = channel(1);
        let (shutdown_sender, shutdown_receiver) = channel(1);

        let handlers = ImapConnectionHandlers {
            connection: Imap::new(idle_interrupt_receiver),
            stop_receiver,
            shutdown_sender,
        };

        let state = ConnectionState {
            idle_interrupt_sender,
            shutdown_receiver,
            stop_sender,
        };

        let conn = ImapConnectionState { state };

        (conn, handlers)
    }

    /// Interrupt any form of idle.
    async fn interrupt(&self) {
        self.state.interrupt().await;
    }

    /// Shutdown this connection completely.
    async fn stop(&self) {
        self.state.stop().await;
    }
}

#[derive(Debug)]
struct ImapConnectionHandlers {
    connection: Imap,
    stop_receiver: Receiver<()>,
    shutdown_sender: Sender<()>,
}

async fn get_watch_folder(context: &Context, config_name: impl AsRef<str>) -> Option<String> {
    match context
        .sql
        .get_raw_config(context, config_name.as_ref())
        .await
    {
        Some(name) => Some(name),
        None => {
            if config_name.as_ref() == "configured_inbox_folder" {
                // initialized with old version, so has not set configured_inbox_folder
                Some("INBOX".to_string())
            } else {
                None
            }
        }
    }
}
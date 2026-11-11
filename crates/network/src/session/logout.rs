#[cfg(test)]
use std::future::Future;
use std::{error::Error as StdError, fmt};

use tokio::{
    sync::oneshot,
    time::{self, Instant},
};

use super::{RuntimeAction, SessionDispatcher, SessionError, SessionState};
use crate::{
    circuit::{LOGOUT_REPLY_TIMEOUT, LOGOUT_TIMEOUT, LogoutCommand, LogoutReceipt},
    error::Error,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LogoutOutcome {
    Received,
    AlreadyReceived,
    Unavailable(LogoutUnavailable),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LogoutUnavailable {
    ConnectionClosed,
    ConnectionEnded,
    SendDeadline,
    ReplyDeadline,
    QueueFull,
}

pub(super) struct PendingLogout {
    completion: Option<oneshot::Receiver<Result<(), Error>>>,
    receipt: oneshot::Receiver<LogoutReceipt>,
    send_deadline: Instant,
    reply_deadline: Option<Instant>,
}

pub(super) enum SessionLogout {
    Active,
    Sending { pending: PendingLogout },
    Received { deadline: Option<Instant> },
    Unavailable { reason: LogoutUnavailable, deadline: Option<Instant> },
}

impl SessionLogout {
    pub(super) fn deadline(&self) -> Option<Instant> {
        match self {
            Self::Active => None,
            Self::Sending { pending } => Some(pending.deadline()),
            Self::Received { deadline } | Self::Unavailable { deadline, .. } => *deadline,
        }
    }

    pub(super) fn is_started(&self) -> bool {
        !matches!(self, Self::Active)
    }

    pub(super) fn is_received(&self) -> bool {
        matches!(self, Self::Received { .. })
    }

    pub(super) fn unavailable(&self) -> Option<LogoutUnavailable> {
        match self {
            Self::Unavailable { reason, .. } => Some(*reason),
            Self::Active | Self::Sending { .. } | Self::Received { .. } => None,
        }
    }

    pub(super) fn pending_mut(&mut self) -> Option<&mut PendingLogout> {
        match self {
            Self::Sending { pending } => Some(pending),
            Self::Active | Self::Received { .. } | Self::Unavailable { .. } => None,
        }
    }
}

impl PendingLogout {
    pub(super) fn new() -> (LogoutCommand, Self) {
        let (sent, completion) = oneshot::channel();
        let (receipt, receipt_receiver) = oneshot::channel();
        (
            LogoutCommand { sent, receipt },
            Self {
                completion: Some(completion),
                receipt: receipt_receiver,
                send_deadline: Instant::now() + LOGOUT_TIMEOUT,
                reply_deadline: None,
            },
        )
    }

    pub(super) fn deadline(&self) -> Instant {
        self.reply_deadline.unwrap_or(self.send_deadline)
    }

    pub(super) async fn wait(&mut self) -> Result<LogoutOutcome, LogoutAttemptError> {
        if let Some(completion) = &mut self.completion {
            match time::timeout_at(self.send_deadline, completion).await {
                Ok(Ok(Ok(()))) => {
                    self.completion = None;
                    self.reply_deadline = Some(Instant::now() + LOGOUT_REPLY_TIMEOUT);
                }
                Ok(Ok(Err(error))) => return Err(logout_completion_error(error)),
                Ok(Err(_)) => return Err(LogoutAttemptError::Unavailable(LogoutUnavailable::ConnectionEnded)),
                Err(_) => return Err(LogoutAttemptError::Unavailable(LogoutUnavailable::SendDeadline)),
            }
        }
        let deadline = self.reply_deadline.unwrap_or(self.send_deadline);
        match time::timeout_at(deadline, &mut self.receipt).await {
            Ok(Ok(LogoutReceipt::Received)) => Ok(LogoutOutcome::Received),
            Ok(Ok(LogoutReceipt::Deadline)) | Err(_) => Err(LogoutAttemptError::Unavailable(LogoutUnavailable::ReplyDeadline)),
            Ok(Ok(LogoutReceipt::ConnectionEnded)) | Ok(Err(_)) => Err(LogoutAttemptError::Unavailable(LogoutUnavailable::ConnectionEnded)),
        }
    }
}

#[derive(Debug)]
pub(crate) enum LogoutAttemptError {
    Unavailable(LogoutUnavailable),
    Unexpected(Error),
}

impl fmt::Display for LogoutAttemptError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(reason) => write!(formatter, "simulator logout was unavailable: {reason:?}"),
            Self::Unexpected(error) => error.fmt(formatter),
        }
    }
}

impl StdError for LogoutAttemptError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Unexpected(error) => Some(error),
            Self::Unavailable(_) => None,
        }
    }
}

impl SessionState {
    pub(super) async fn disconnect_once(&mut self) -> Result<LogoutOutcome, SessionError> {
        tracing::info!("simulator disconnect started");
        let outcome = if self.logout.is_received() {
            LogoutOutcome::AlreadyReceived
        } else if let Some(reason) = self.logout.unavailable() {
            LogoutOutcome::Unavailable(reason)
        } else if self.logout.is_started() {
            self.finish_pending_logout().await?;
            if self.logout.is_received() {
                LogoutOutcome::AlreadyReceived
            } else {
                LogoutOutcome::Unavailable(self.logout.unavailable().unwrap_or(LogoutUnavailable::ConnectionEnded))
            }
        } else if !self.primary().is_current() {
            self.record_logout_unavailable(LogoutUnavailable::ConnectionClosed, None);
            LogoutOutcome::Unavailable(LogoutUnavailable::ConnectionClosed)
        } else {
            let (command, pending) = PendingLogout::new();
            let deadline = Some(pending.deadline());
            match time::timeout_at(pending.deadline(), self.primary().logout.send(command)).await {
                Err(_) => {
                    self.record_logout_unavailable(LogoutUnavailable::SendDeadline, deadline);
                    LogoutOutcome::Unavailable(LogoutUnavailable::SendDeadline)
                }
                Ok(Err(_)) => {
                    self.record_logout_unavailable(LogoutUnavailable::ConnectionClosed, deadline);
                    LogoutOutcome::Unavailable(LogoutUnavailable::ConnectionClosed)
                }
                Ok(Ok(())) => {
                    self.logout = SessionLogout::Sending { pending };
                    self.finish_pending_logout().await?;
                    if self.logout.is_received() {
                        LogoutOutcome::Received
                    } else {
                        LogoutOutcome::Unavailable(self.logout.unavailable().unwrap_or(LogoutUnavailable::ConnectionEnded))
                    }
                }
            }
        };
        match outcome {
            LogoutOutcome::Received => {
                tracing::debug!(local_send_completed = true, remote_receipt_confirmed = true, "simulator logout reply received");
            }
            LogoutOutcome::AlreadyReceived => {
                tracing::debug!(
                    local_send_completed = true,
                    remote_receipt_confirmed = true,
                    "simulator logout reply was already received"
                );
            }
            LogoutOutcome::Unavailable(reason) => {
                self.record_logout_unavailable(reason, self.logout.deadline());
            }
        }
        Ok(outcome)
    }

    pub(super) fn apply_logout_attempt_result(
        &mut self,
        result: Result<LogoutOutcome, LogoutAttemptError>,
        deadline: Option<Instant>,
    ) -> Result<(), SessionError> {
        match result {
            Ok(LogoutOutcome::Received) => {
                self.logout = SessionLogout::Received { deadline };
                Ok(())
            }
            Ok(LogoutOutcome::AlreadyReceived) => {
                self.logout = SessionLogout::Received { deadline };
                Ok(())
            }
            Ok(LogoutOutcome::Unavailable(reason)) | Err(LogoutAttemptError::Unavailable(reason)) => {
                self.record_logout_unavailable(reason, deadline);
                Ok(())
            }
            Err(LogoutAttemptError::Unexpected(error)) => Err(error.into()),
        }
    }

    pub(super) async fn finish_pending_logout(&mut self) -> Result<(), SessionError> {
        let state = std::mem::replace(&mut self.logout, SessionLogout::Active);
        match state {
            SessionLogout::Sending { mut pending } => {
                let result = pending.wait().await;
                self.apply_logout_attempt_result(result, Some(pending.deadline()))?;
            }
            state => self.logout = state,
        }
        Ok(())
    }

    pub(super) fn record_logout_unavailable(&mut self, reason: LogoutUnavailable, deadline: Option<Instant>) {
        if self.logout.unavailable() == Some(reason) && self.logout.deadline() == deadline {
            return;
        }
        self.logout = SessionLogout::Unavailable { reason, deadline };
        tracing::warn!(simulator = self.primary_id.get(), ?reason, "simulator disconnect completed without a logout reply");
    }

    pub(super) async fn graceful_disconnect(&mut self) -> Result<(), SessionError> {
        let result = self.disconnect_once().await;
        self.neighbor_cancellation.send_replace(true);
        result.map(|_| ())
    }
}

impl SessionDispatcher<'_> {
    pub(super) async fn handle_disconnect_changed(&mut self) -> Result<RuntimeAction, SessionError> {
        self.state.graceful_disconnect().await?;
        Ok(RuntimeAction::Exit(Ok(())))
    }

    pub(super) fn handle_primary_logout_completed(
        &mut self,
        result: Result<LogoutOutcome, LogoutAttemptError>,
    ) -> Result<RuntimeAction, SessionError> {
        let deadline = self.state.logout.deadline();
        self.state.apply_logout_attempt_result(result, deadline)?;
        Ok(RuntimeAction::Exit(Ok(())))
    }

    pub(super) async fn handle_event_receiver_closed(&mut self) -> Result<RuntimeAction, SessionError> {
        self.state.graceful_disconnect().await?;
        Ok(RuntimeAction::Exit(Ok(())))
    }
}

fn logout_completion_error(error: Error) -> LogoutAttemptError {
    match error {
        Error::SimulatorLogoutSendAgeExpired => LogoutAttemptError::Unavailable(LogoutUnavailable::SendDeadline),
        error => LogoutAttemptError::Unexpected(error),
    }
}

#[cfg(test)]
pub(super) async fn send_logout_with_deadline(
    send: impl Future<Output = Result<(), LogoutAttemptError>>,
    deadline_reason: LogoutUnavailable,
) -> Result<(), LogoutAttemptError> {
    time::timeout(LOGOUT_TIMEOUT, send).await.map_err(|_| LogoutAttemptError::Unavailable(deadline_reason))?
}

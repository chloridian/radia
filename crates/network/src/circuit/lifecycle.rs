use std::mem;

use tokio::{sync::oneshot, time::Instant};

use crate::error::Error;

use super::{ConnectionError, LOGOUT_REPLY_TIMEOUT, LOGOUT_TIMEOUT, LogoutCommand, LogoutReceipt};

pub(super) enum Phase {
    Connecting { region_handshake_seen: bool },
    Established { region_handshake_seen: bool },
    Closing { stage: ConnectionStage, logout: LogoutState, deadline: Instant, reason: Option<&'static str> },
    Draining { stage: ConnectionStage, logout: LogoutState, deadline: Instant, reason: Option<&'static str> },
}

#[derive(Clone, Copy)]
pub(super) enum ConnectionStage {
    Connecting { region_handshake_seen: bool },
    Established { region_handshake_seen: bool },
}

pub(super) enum LogoutState {
    NotRequested,
    AwaitingLocalSend { completions: Vec<oneshot::Sender<Result<(), Error>>>, receipts: Vec<oneshot::Sender<LogoutReceipt>> },
    AwaitingReply(Vec<oneshot::Sender<LogoutReceipt>>),
    Complete(LogoutReceipt),
}

pub(super) enum LogoutAction {
    Queue(oneshot::Sender<Result<(), Error>>),
    Complete(oneshot::Sender<Result<(), Error>>),
    Waiting,
}

pub(super) enum RunControl {
    Continue,
    Finish(Option<&'static str>),
}

impl Phase {
    pub(super) fn new() -> Self {
        Self::Connecting { region_handshake_seen: false }
    }

    pub(super) fn is_established(&self) -> bool {
        matches!(self.stage(), ConnectionStage::Established { .. })
    }

    pub(super) fn is_closing(&self) -> bool {
        matches!(self, Self::Closing { .. } | Self::Draining { .. })
    }

    pub(super) fn is_draining(&self) -> bool {
        matches!(self, Self::Draining { .. })
    }

    pub(super) fn has_region_handshake(&self) -> bool {
        match self.stage() {
            ConnectionStage::Connecting { region_handshake_seen } | ConnectionStage::Established { region_handshake_seen } => {
                region_handshake_seen
            }
        }
    }

    pub(super) fn mark_region_handshake(&mut self) {
        match self {
            Self::Connecting { region_handshake_seen }
            | Self::Established { region_handshake_seen }
            | Self::Closing { stage: ConnectionStage::Connecting { region_handshake_seen }, .. }
            | Self::Closing { stage: ConnectionStage::Established { region_handshake_seen }, .. }
            | Self::Draining { stage: ConnectionStage::Connecting { region_handshake_seen }, .. }
            | Self::Draining { stage: ConnectionStage::Established { region_handshake_seen }, .. } => {
                *region_handshake_seen = true;
            }
        }
    }

    pub(super) fn mark_established(&mut self) -> bool {
        match self {
            Self::Connecting { region_handshake_seen } => {
                *self = Self::Established { region_handshake_seen: *region_handshake_seen };
                true
            }
            Self::Established { .. } => false,
            Self::Closing { stage, .. } | Self::Draining { stage, .. } => match stage {
                ConnectionStage::Connecting { region_handshake_seen } => {
                    *stage = ConnectionStage::Established { region_handshake_seen: *region_handshake_seen };
                    true
                }
                ConnectionStage::Established { .. } => false,
            },
        }
    }

    pub(super) fn close(&mut self, reason: Option<&'static str>, reset_deadline: bool) {
        let deadline = Instant::now() + LOGOUT_TIMEOUT;
        match self {
            Self::Connecting { region_handshake_seen } => {
                *self = Self::Closing {
                    stage: ConnectionStage::Connecting { region_handshake_seen: *region_handshake_seen },
                    logout: LogoutState::NotRequested,
                    deadline,
                    reason,
                };
            }
            Self::Established { region_handshake_seen } => {
                *self = Self::Closing {
                    stage: ConnectionStage::Established { region_handshake_seen: *region_handshake_seen },
                    logout: LogoutState::NotRequested,
                    deadline,
                    reason,
                };
            }
            Self::Closing { deadline: current_deadline, reason: current_reason, .. }
            | Self::Draining { deadline: current_deadline, reason: current_reason, .. } => {
                if reset_deadline {
                    *current_deadline = deadline;
                }
                if let Some(reason) = reason {
                    *current_reason = Some(reason);
                }
            }
        }
    }

    pub(super) fn begin_draining(&mut self) {
        let transition = match self {
            Self::Closing { stage, logout, deadline, reason } => {
                Some((*stage, mem::replace(logout, LogoutState::NotRequested), *deadline, *reason))
            }
            Self::Connecting { .. } | Self::Established { .. } | Self::Draining { .. } => None,
        };
        if let Some((stage, logout, deadline, reason)) = transition {
            *self = Self::Draining { stage, logout, deadline, reason };
        }
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        match self {
            Self::Closing { deadline, .. } | Self::Draining { deadline, .. } => Some(*deadline),
            Self::Connecting { .. } | Self::Established { .. } => None,
        }
    }

    pub(super) fn close_reason(&self) -> Option<&'static str> {
        match self {
            Self::Closing { reason, .. } | Self::Draining { reason, .. } => *reason,
            Self::Connecting { .. } | Self::Established { .. } => None,
        }
    }

    pub(super) fn logout_requested(&self) -> bool {
        matches!(
            self.logout_state(),
            Some(LogoutState::AwaitingLocalSend { .. } | LogoutState::AwaitingReply(_) | LogoutState::Complete(_))
        )
    }

    pub(super) fn request_logout(&mut self, command: LogoutCommand) -> LogoutAction {
        if !self.is_closing() {
            self.close(None, false);
        }
        match self {
            Self::Closing { logout, .. } | Self::Draining { logout, .. } => match logout {
                LogoutState::NotRequested => {
                    *logout = LogoutState::AwaitingLocalSend { completions: Vec::new(), receipts: vec![command.receipt] };
                    LogoutAction::Queue(command.sent)
                }
                LogoutState::AwaitingLocalSend { completions, receipts } => {
                    completions.push(command.sent);
                    receipts.push(command.receipt);
                    LogoutAction::Waiting
                }
                LogoutState::AwaitingReply(receipts) => {
                    receipts.push(command.receipt);
                    LogoutAction::Complete(command.sent)
                }
                LogoutState::Complete(receipt) => {
                    complete_sender(command.sent, Ok(()));
                    complete_receipt(command.receipt, *receipt);
                    LogoutAction::Waiting
                }
            },
            Self::Connecting { .. } | Self::Established { .. } => unreachable!("logout closes the connection"),
        }
    }

    pub(super) fn complete_logout(&mut self, error: Option<ConnectionError>) {
        let Some(logout) = self.logout_state_mut() else { return };
        let state = mem::replace(logout, LogoutState::NotRequested);
        let LogoutState::AwaitingLocalSend { completions, receipts } = state else {
            *logout = state;
            return;
        };
        if let Some(error) = error {
            *logout = LogoutState::Complete(LogoutReceipt::ConnectionEnded);
            for receipt in receipts {
                complete_receipt(receipt, LogoutReceipt::ConnectionEnded);
            }
            for completion in completions {
                complete_sender(completion, Err(error.to_network_error()));
            }
        } else {
            *logout = LogoutState::AwaitingReply(receipts);
            self.set_deadline(Instant::now() + LOGOUT_REPLY_TIMEOUT);
            for completion in completions {
                complete_sender(completion, Ok(()));
            }
        }
    }

    pub(super) fn complete_logout_receipt(&mut self, receipt: LogoutReceipt) {
        let Some(logout) = self.logout_state_mut() else { return };
        let state = mem::replace(logout, LogoutState::NotRequested);
        if let LogoutState::AwaitingReply(receipts) = state {
            *logout = LogoutState::Complete(receipt);
            for sender in receipts {
                complete_receipt(sender, receipt);
            }
        } else {
            *logout = state;
        }
    }

    pub(super) fn expire_logout(&mut self) {
        match self.logout_state() {
            Some(LogoutState::AwaitingLocalSend { .. }) => self.complete_logout(Some(ConnectionError::SendAgeExpired { logout: true })),
            Some(LogoutState::AwaitingReply(_)) => self.complete_logout_receipt(LogoutReceipt::Deadline),
            Some(LogoutState::NotRequested | LogoutState::Complete(_)) | None => {}
        }
    }

    pub(super) fn connection_ended(&mut self) {
        let Some(logout) = self.logout_state_mut() else { return };
        let state = mem::replace(logout, LogoutState::NotRequested);
        let receipts = match state {
            LogoutState::AwaitingLocalSend { receipts, .. } | LogoutState::AwaitingReply(receipts) => {
                *logout = LogoutState::Complete(LogoutReceipt::ConnectionEnded);
                receipts
            }
            state @ (LogoutState::NotRequested | LogoutState::Complete(_)) => {
                *logout = state;
                return;
            }
        };
        for sender in receipts {
            complete_receipt(sender, LogoutReceipt::ConnectionEnded);
        }
    }

    fn set_deadline(&mut self, deadline: Instant) {
        match self {
            Self::Closing { deadline: current, .. } | Self::Draining { deadline: current, .. } => *current = deadline,
            Self::Connecting { .. } | Self::Established { .. } => {}
        }
    }

    fn stage(&self) -> ConnectionStage {
        match self {
            Self::Connecting { region_handshake_seen } => ConnectionStage::Connecting { region_handshake_seen: *region_handshake_seen },
            Self::Established { region_handshake_seen } => ConnectionStage::Established { region_handshake_seen: *region_handshake_seen },
            Self::Closing { stage, .. } | Self::Draining { stage, .. } => *stage,
        }
    }

    fn logout_state(&self) -> Option<&LogoutState> {
        match self {
            Self::Closing { logout, .. } | Self::Draining { logout, .. } => Some(logout),
            Self::Connecting { .. } | Self::Established { .. } => None,
        }
    }

    fn logout_state_mut(&mut self) -> Option<&mut LogoutState> {
        match self {
            Self::Closing { logout, .. } | Self::Draining { logout, .. } => Some(logout),
            Self::Connecting { .. } | Self::Established { .. } => None,
        }
    }
}

fn complete_sender(sender: oneshot::Sender<Result<(), Error>>, result: Result<(), Error>) {
    if let Err(result) = sender.send(result) {
        drop(result);
    }
}

fn complete_receipt(sender: oneshot::Sender<LogoutReceipt>, receipt: LogoutReceipt) {
    if sender.send(receipt).is_err() {}
}

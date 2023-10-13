use thiserror::Error;

#[derive(Debug, Eq, PartialEq, Error)]
pub enum SchedulerError {
    #[error("Sending channel disconnected: {0}")]
    DisconnectedSendChannel(&'static str),
    #[error("Receiving channel disconnected: {0}")]
    DisconnectedReceiveChannel(&'static str),
    #[error("Recv channel disconnected: {0}")]
    DisconnectedRecvChannel(&'static str),
}

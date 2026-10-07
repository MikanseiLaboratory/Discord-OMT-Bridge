use thiserror::Error;

#[derive(Debug, Error)]
pub enum EgressError {
    #[error("OMT: {0}")]
    Omt(String),
    #[error("NDI: {0}")]
    Ndi(String),
    #[error("invalid frame: {0}")]
    InvalidFrame(&'static str),
}

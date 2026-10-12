use std::fmt;

#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

pub fn bad<T>(message: impl Into<String>) -> ApiResult<T> {
    Err(ApiError::new(400, message))
}

pub fn status<T>(code: u16, message: impl Into<String>) -> ApiResult<T> {
    Err(ApiError::new(code, message))
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.status, self.message)
    }
}

impl From<worker::Error> for ApiError {
    fn from(e: worker::Error) -> Self {
        worker::console_error!("internal error: {e}");
        ApiError::new(500, "internal error")
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        ApiError::new(400, format!("invalid JSON: {e}"))
    }
}

impl From<collective_core::challenge::ChallengeError> for ApiError {
    fn from(e: collective_core::challenge::ChallengeError) -> Self {
        ApiError::new(409, e.0)
    }
}

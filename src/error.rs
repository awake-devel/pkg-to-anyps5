use std::fmt;

/// Every failure is fatal: a package that does not match the expected layout
/// is reported, never extracted partially or guessed around.
#[derive(Debug)]
pub enum Error {
    Io(String, std::io::Error),
    Format(String),
    Unsupported(String),
}

impl Error {
    pub fn format(message: impl Into<String>) -> Self {
        Error::Format(message.into())
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Error::Unsupported(message.into())
    }

    pub fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> Self {
        let context = context.into();
        move |err| Error::Io(context, err)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(context, err) => write!(f, "{context}: {err}"),
            Error::Format(message) => write!(f, "invalid package: {message}"),
            Error::Unsupported(message) => write!(f, "unsupported package: {message}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

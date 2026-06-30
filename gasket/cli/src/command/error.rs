//! Build-time errors for the dispatcher builder.

use std::io;

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("CommandHost not set; call .host() before build()")]
    MissingHost,

    #[error("duplicate built-in name: /{0}")]
    DuplicateBuiltin(String),

    #[error("user_dir is set but cannot be read: {0}")]
    UserDirIO(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_host_message() {
        let e = BuildError::MissingHost;
        assert_eq!(
            e.to_string(),
            "CommandHost not set; call .host() before build()"
        );
    }

    #[test]
    fn duplicate_builtin_message() {
        let e = BuildError::DuplicateBuiltin("help".into());
        assert_eq!(e.to_string(), "duplicate built-in name: /help");
    }

    #[test]
    fn from_io_error() {
        let io_err = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        let e: BuildError = io_err.into();
        match e {
            BuildError::UserDirIO(inner) => {
                assert_eq!(inner.kind(), io::ErrorKind::PermissionDenied);
            }
            _ => panic!("unexpected variant"),
        }
    }
}

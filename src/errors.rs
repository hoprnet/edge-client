use thiserror::Error;

#[derive(Error, Debug)]
pub enum EdgliError {
    #[error("file error: '{0}'")]
    FileError(String),

    #[error("configuration error: '{0}'")]
    ConfigError(String),

    #[error("serialization failed: '{0}'")]
    SerializationError(String),

    #[error("validation failed: '{0}'")]
    ValidationError(String),

    #[error(transparent)]
    HoprLibError(#[from] hopr_lib::errors::HoprLibError),

    #[error("os error: '{0}'")]
    OsError(String),
}

pub type Result<T> = std::result::Result<T, EdgliError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_error_display() {
        let e = EdgliError::FileError("not found".into());
        assert_eq!(e.to_string(), "file error: 'not found'");
    }

    #[test]
    fn config_error_display() {
        let e = EdgliError::ConfigError("bad value".into());
        assert_eq!(e.to_string(), "configuration error: 'bad value'");
    }

    #[test]
    fn serialization_error_display() {
        let e = EdgliError::SerializationError("parse failed".into());
        assert_eq!(e.to_string(), "serialization failed: 'parse failed'");
    }

    #[test]
    fn validation_error_display() {
        let e = EdgliError::ValidationError("out of range".into());
        assert_eq!(e.to_string(), "validation failed: 'out of range'");
    }

    #[test]
    fn os_error_display() {
        let e = EdgliError::OsError("signal failed".into());
        assert_eq!(e.to_string(), "os error: 'signal failed'");
    }

    #[test]
    fn hopr_lib_error_converts_via_from() {
        let hopr_err = hopr_lib::errors::HoprLibError::GeneralError("something broke".into());
        let edgli_err = EdgliError::from(hopr_err);
        assert!(
            edgli_err.to_string().contains("something broke"),
            "transparent display must include underlying message"
        );
    }

    #[test]
    fn result_alias_is_edgli_error() {
        let r: Result<()> = Err(EdgliError::ConfigError("x".into()));
        assert!(r.is_err());
    }
}

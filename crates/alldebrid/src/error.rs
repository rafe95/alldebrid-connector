//! Error types.

use std::fmt;
use std::time::Duration;

use serde::Deserialize;

use crate::types::{MagnetId, StatusCode};

/// Convenient alias for `Result<T, alldebrid::Error>`.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong when talking to AllDebrid.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The API answered with `"status": "error"`.
    #[error(transparent)]
    Api(#[from] ApiError),

    /// The API answered with a non-2xx HTTP status and no usable JSON body.
    #[error("HTTP {status}: {body}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// Raw response body (possibly truncated).
        body: String,
    },

    /// Transport-level failure (DNS, TLS, connection reset, timeout...).
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    /// The response could not be decoded into the expected type.
    #[error("could not decode response: {source}")]
    Decode {
        /// The underlying JSON error.
        #[source]
        source: serde_json::Error,
        /// Raw response body (possibly truncated).
        body: String,
    },

    /// Still rate limited (HTTP 429/503) after exhausting all retries.
    #[error("rate limited by AllDebrid")]
    RateLimited {
        /// Value of the `Retry-After` header, if any.
        retry_after: Option<Duration>,
    },

    /// A magnet reached a terminal error status (codes 5..=15).
    #[error("magnet {id} failed with status {code:?}: {message}")]
    MagnetFailed {
        /// The magnet that failed.
        id: MagnetId,
        /// Its terminal status code.
        code: StatusCode,
        /// Human readable status reported by AllDebrid.
        message: String,
    },

    /// Delayed link generation reported an error (status 3).
    #[error("delayed link generation failed")]
    DelayedLinkFailed,

    /// A response was well formed but did not contain what was expected.
    #[error("unexpected response: {0}")]
    UnexpectedResponse(String),

    /// A helper gave up waiting.
    #[error("timed out")]
    Timeout,

    /// Invalid client configuration.
    #[error("invalid configuration: {0}")]
    Config(String),
}

impl Error {
    /// The API error code, if this is an [`Error::Api`].
    pub fn api_code(&self) -> Option<&ApiErrorCode> {
        match self {
            Error::Api(e) => Some(&e.code),
            _ => None,
        }
    }

    /// `true` if this error means the API key is missing, wrong, blocked or banned.
    pub fn is_auth(&self) -> bool {
        self.api_code().is_some_and(ApiErrorCode::is_auth)
    }
}

/// An error returned by the API in its `{"status":"error","error":{...}}` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ApiError {
    /// Machine readable code, e.g. `MAGNET_INVALID_URI`.
    pub code: ApiErrorCode,
    /// Human readable message from AllDebrid.
    #[serde(default)]
    pub message: String,
}

macro_rules! api_error_codes {
    ($($(#[$meta:meta])* $variant:ident => $code:literal),* $(,)?) => {
        /// Error codes documented by AllDebrid.
        ///
        /// Unknown codes are preserved in [`ApiErrorCode::Other`], so new codes
        /// added by AllDebrid never break deserialization.
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum ApiErrorCode {
            $($(#[$meta])* #[doc = concat!("`", $code, "`")] $variant,)*
            /// A code this crate does not know about yet.
            Other(String),
        }

        impl ApiErrorCode {
            /// The code as sent by the API.
            pub fn as_str(&self) -> &str {
                match self {
                    $(ApiErrorCode::$variant => $code,)*
                    ApiErrorCode::Other(s) => s,
                }
            }

            /// Parse a code as sent by the API.
            pub fn from_code(code: &str) -> Self {
                match code {
                    $($code => ApiErrorCode::$variant,)*
                    other => ApiErrorCode::Other(other.to_owned()),
                }
            }
        }
    };
}

api_error_codes! {
    Generic => "GENERIC",
    NotFound => "404",
    AuthMissingApikey => "AUTH_MISSING_APIKEY",
    AuthBadApikey => "AUTH_BAD_APIKEY",
    /// Access from a new location must be approved by email.
    AuthBlocked => "AUTH_BLOCKED",
    AuthUserBanned => "AUTH_USER_BANNED",
    Discontinued => "DISCONTINUED",
    NoServer => "NO_SERVER",
    MustBePremium => "MUST_BE_PREMIUM",
    FreeTrialLimitReached => "FREE_TRIAL_LIMIT_REACHED",
    LinkIsMissing => "LINK_IS_MISSING",
    BadLink => "BAD_LINK",
    LinkHostNotSupported => "LINK_HOST_NOT_SUPPORTED",
    LinkDown => "LINK_DOWN",
    LinkPassProtected => "LINK_PASS_PROTECTED",
    LinkHostUnavailable => "LINK_HOST_UNAVAILABLE",
    LinkTooManyDownloads => "LINK_TOO_MANY_DOWNLOADS",
    LinkHostFull => "LINK_HOST_FULL",
    LinkHostLimitReached => "LINK_HOST_LIMIT_REACHED",
    LinkError => "LINK_ERROR",
    LinkTemporaryUnavailable => "LINK_TEMPORARY_UNAVAILABLE",
    LinkNotSupported => "LINK_NOT_SUPPORTED",
    RedirectorNotSupported => "REDIRECTOR_NOT_SUPPORTED",
    RedirectorError => "REDIRECTOR_ERROR",
    StreamInvalidGenId => "STREAM_INVALID_GEN_ID",
    StreamInvalidStreamId => "STREAM_INVALID_STREAM_ID",
    DelayedInvalidId => "DELAYED_INVALID_ID",
    MagnetNoUri => "MAGNET_NO_URI",
    MagnetInvalidUri => "MAGNET_INVALID_URI",
    MagnetInvalidFile => "MAGNET_INVALID_FILE",
    MagnetFileUploadFailed => "MAGNET_FILE_UPLOAD_FAILED",
    MagnetMustBePremium => "MAGNET_MUST_BE_PREMIUM",
    MagnetNoServer => "MAGNET_NO_SERVER",
    MagnetTooLarge => "MAGNET_TOO_LARGE",
    /// At most 30 magnets can be active at the same time.
    MagnetTooManyActive => "MAGNET_TOO_MANY_ACTIVE",
    MagnetTooMany => "MAGNET_TOO_MANY",
    MagnetInvalidId => "MAGNET_INVALID_ID",
    MagnetProcessing => "MAGNET_PROCESSING",
    PinAlreadyAuthed => "PIN_ALREADY_AUTHED",
    PinExpired => "PIN_EXPIRED",
    PinInvalid => "PIN_INVALID",
    AlreadySent => "ALREADY_SENT",
    UserLinkMissing => "USER_LINK_MISSING",
    UserLinkInvalid => "USER_LINK_INVALID",
    NoJsonParam => "NO_JSON_PARAM",
    JsonInvalid => "JSON_INVALID",
}

impl ApiErrorCode {
    /// Authentication problems: retrying will not help.
    pub fn is_auth(&self) -> bool {
        matches!(
            self,
            ApiErrorCode::AuthMissingApikey
                | ApiErrorCode::AuthBadApikey
                | ApiErrorCode::AuthBlocked
                | ApiErrorCode::AuthUserBanned
        )
    }

    /// Server-side capacity problems that are worth retrying later.
    pub fn is_retryable(&self) -> bool {
        matches!(self, ApiErrorCode::NoServer | ApiErrorCode::MagnetNoServer)
    }
}

impl fmt::Display for ApiErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ApiErrorCode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(ApiErrorCode::from_code(&s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_and_unknown_codes_round_trip() {
        assert_eq!(
            ApiErrorCode::from_code("MAGNET_INVALID_URI"),
            ApiErrorCode::MagnetInvalidUri
        );
        assert_eq!(
            ApiErrorCode::MagnetInvalidUri.as_str(),
            "MAGNET_INVALID_URI"
        );
        let other = ApiErrorCode::from_code("SOMETHING_NEW");
        assert_eq!(other, ApiErrorCode::Other("SOMETHING_NEW".into()));
        assert_eq!(other.as_str(), "SOMETHING_NEW");
    }

    #[test]
    fn deserializes_api_error() {
        let e: ApiError =
            serde_json::from_str(r#"{"code":"AUTH_BAD_APIKEY","message":"bad key"}"#).unwrap();
        assert!(e.code.is_auth());
        assert_eq!(e.to_string(), "AUTH_BAD_APIKEY: bad key");
    }
}

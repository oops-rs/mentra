use std::time::Duration;

use thiserror::Error;
use time::{OffsetDateTime, PrimitiveDateTime};

/// Errors returned by provider implementations and stream adapters.
#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider transport error: {0}")]
    Transport(#[source] reqwest::Error),
    #[error("retryable provider error: {message}")]
    Retryable {
        message: String,
        delay: Option<Duration>,
    },
    #[error("provider does not support capability: {0}")]
    UnsupportedCapability(String),
    #[error("{message}", message = provider_http_error(.status, .body))]
    Http {
        status: reqwest::StatusCode,
        body: String,
        /// How long the server asked the caller to wait, from the response's
        /// `Retry-After` header, or `None` when it sent none.
        ///
        /// A rate limit is the one failure whose recovery time the server
        /// knows and the client cannot guess: an exponential backoff shaped
        /// for a connection blip retries five times inside the window and
        /// then gives up while the limit is still in force. Capturing the
        /// header here, where the response is turned into an error, is what
        /// lets a caller wait the interval the server named instead —
        /// nothing further up the stack ever sees the headers.
        ///
        /// Read it back through [`retry_after`](ProviderError::retry_after),
        /// which answers the same question for
        /// [`Retryable`](ProviderError::Retryable) too. Build one of these
        /// from a live response with
        /// [`from_http_response`](ProviderError::from_http_response) rather
        /// than filling the field in by hand.
        retry_after: Option<Duration>,
    },
    #[error("failed to decode provider response: {0}")]
    Decode(#[source] reqwest::Error),
    #[error("failed to serialize provider request: {0}")]
    Serialize(#[source] serde_json::Error),
    #[error("failed to deserialize provider payload: {0}")]
    Deserialize(#[source] serde_json::Error),
    #[error("invalid provider request: {0}")]
    InvalidRequest(String),
    #[error("invalid provider response: {0}")]
    InvalidResponse(String),
    #[error("malformed provider stream: {0}")]
    MalformedStream(String),
}

impl ProviderError {
    /// Turns an unsuccessful HTTP response into an [`Http`](ProviderError::Http)
    /// error, reading `Retry-After` before the body is consumed.
    ///
    /// This is the constructor every provider in this crate uses, and the one
    /// a custom provider should use: the status and the retry hint both come
    /// off the response, so neither can be forgotten at a call site.
    pub async fn from_http_response(response: reqwest::Response) -> Self {
        let status = response.status();
        let retry_after = retry_after_from_headers(response.headers());
        Self::Http {
            status,
            body: response.text().await.unwrap_or_default(),
            retry_after,
        }
    }

    /// How long the provider asked the caller to wait before trying again, or
    /// `None` when it asked for nothing.
    ///
    /// This is a request from the server, not a promise about the schedule: a
    /// caller decides what to do with it, including refusing an interval long
    /// enough to be an outage rather than a rate limit.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Http { retry_after, .. } => *retry_after,
            Self::Retryable { delay, .. } => *delay,
            _ => None,
        }
    }
}

fn provider_http_error(status: &reqwest::StatusCode, body: &str) -> String {
    if body.trim().is_empty() {
        format!("provider returned HTTP {status}")
    } else {
        format!("provider returned HTTP {status}: {body}")
    }
}

/// Reads `Retry-After` off a response's headers, in whichever of its two forms
/// the server chose.
pub(crate) fn retry_after_from_headers(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    retry_after_from_header_value(headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?)
}

/// Reads one already-extracted `Retry-After` value, for transports that carry
/// their headers as something other than a [`HeaderMap`](reqwest::header::HeaderMap).
pub(crate) fn retry_after_from_header_value(value: &str) -> Option<Duration> {
    parse_retry_after(value, OffsetDateTime::now_utc())
}

/// Parses a `Retry-After` value against a known `now`.
///
/// RFC 9110 allows two spellings — a count of seconds, and an HTTP-date — and
/// providers use both, so a parser that understands only one silently ignores
/// half the rate limits it is meant to honor. `now` is a parameter rather than
/// read from the clock so the date form can be tested without waiting.
///
/// A date already in the past yields [`Duration::ZERO`] ("retry now") rather
/// than nothing: the server did answer, and the answer was that the wait is
/// over. Only the IMF-fixdate spelling that RFC 9110 requires senders to emit
/// is understood; the two obsolete formats it only requires recipients to
/// tolerate are read as no hint at all, which costs nothing but the schedule's
/// own delay.
fn parse_retry_after(value: &str, now: OffsetDateTime) -> Option<Duration> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }

    let deadline = parse_http_date(value)?;
    Some((deadline - now).try_into().unwrap_or(Duration::ZERO))
}

/// Parses an IMF-fixdate, the `Sun, 06 Nov 1994 08:49:37 GMT` spelling.
fn parse_http_date(value: &str) -> Option<OffsetDateTime> {
    // `parse_borrowed` rather than `parse`: the latter is deprecated from
    // time 0.3.55 and a downstream resolving a newer `time` would see the
    // warning in this crate. Version 2 of the description syntax, which the
    // spelling below is already written in.
    let format = time::format_description::parse_borrowed::<2>(
        "[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT",
    )
    .ok()?;
    PrimitiveDateTime::parse(value, format.as_slice())
        .ok()
        .map(PrimitiveDateTime::assume_utc)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The instant in RFC 9110's own `Retry-After` example, built without the
    /// `time` macros feature so the crate's dependency set stays as it is.
    fn now() -> OffsetDateTime {
        time::Date::from_calendar_date(1994, time::Month::November, 6)
            .expect("a real date")
            .with_hms(8, 49, 37)
            .expect("a real time")
            .assume_utc()
    }

    #[test]
    fn a_delay_in_seconds_is_read_as_seconds() {
        assert_eq!(
            parse_retry_after("30", now()),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            parse_retry_after("  30  ", now()),
            Some(Duration::from_secs(30)),
            "surrounding whitespace is not part of the value"
        );
    }

    #[test]
    fn an_http_date_is_read_as_the_wait_until_it() {
        // The other spelling RFC 9110 allows. A parser that understood only
        // seconds would return None here and fall back to its own schedule,
        // which is exactly the rate limit it was supposed to wait out.
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:50:37 GMT", now()),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn an_http_date_that_has_passed_means_retry_now() {
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:00 GMT", now()),
            Some(Duration::ZERO),
            "the server answered; the answer is that the wait is over"
        );
    }

    #[test]
    fn an_unparseable_value_is_no_hint_at_all() {
        assert_eq!(parse_retry_after("", now()), None);
        assert_eq!(parse_retry_after("soon", now()), None);
        assert_eq!(parse_retry_after("-5", now()), None);
    }

    #[test]
    fn a_rate_limited_response_reports_the_interval_it_named() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "42".parse().expect("valid"));

        assert_eq!(
            retry_after_from_headers(&headers),
            Some(Duration::from_secs(42))
        );
    }

    #[tokio::test]
    async fn a_429_response_becomes_an_error_that_still_knows_the_interval() {
        // The whole point of reading the header at construction time: nothing
        // above this call ever sees the response, so a hint not captured here
        // is a hint lost.
        let response = http::Response::builder()
            .status(429)
            .header("retry-after", "60")
            .body("rate limit exceeded")
            .expect("a response");

        let error = ProviderError::from_http_response(reqwest::Response::from(response)).await;

        let ProviderError::Http {
            status,
            body,
            retry_after,
        } = error
        else {
            panic!("an unsuccessful response is an Http error");
        };
        assert_eq!(status, reqwest::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body, "rate limit exceeded");
        assert_eq!(retry_after, Some(Duration::from_secs(60)));
    }

    #[tokio::test]
    async fn a_response_without_the_header_asks_for_nothing() {
        let response = http::Response::builder()
            .status(503)
            .body("upstream is restarting")
            .expect("a response");

        let error = ProviderError::from_http_response(reqwest::Response::from(response)).await;

        assert_eq!(error.retry_after(), None);
    }

    #[test]
    fn both_retryable_shapes_answer_the_same_question() {
        // A caller shaping a backoff asks one question and must not have to
        // know which variant carried the answer.
        let http = ProviderError::Http {
            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
            body: String::new(),
            retry_after: Some(Duration::from_secs(20)),
        };
        let retryable = ProviderError::Retryable {
            message: "connection closed".to_string(),
            delay: Some(Duration::from_millis(750)),
        };
        let silent = ProviderError::InvalidRequest("bad model".to_string());

        assert_eq!(http.retry_after(), Some(Duration::from_secs(20)));
        assert_eq!(retryable.retry_after(), Some(Duration::from_millis(750)));
        assert_eq!(silent.retry_after(), None);
    }
}

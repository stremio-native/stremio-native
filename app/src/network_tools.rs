use std::time::{Duration, Instant};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpeedTestDisclosure {
    endpoint: url::Url,
}

impl SpeedTestDisclosure {
    pub fn accept(endpoint: &str) -> Result<Self, NetworkToolError> {
        let endpoint = url::Url::parse(endpoint).map_err(|_| NetworkToolError::InvalidEndpoint)?;
        if endpoint.scheme() != "https" {
            return Err(NetworkToolError::InvalidEndpoint);
        }
        Ok(Self { endpoint })
    }

    pub fn endpoint_host(&self) -> Option<&str> {
        self.endpoint.host_str()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SpeedTestResult {
    pub bytes_received: u64,
    pub elapsed: Duration,
    pub bits_per_second: f64,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum NetworkToolError {
    #[error("speed-test endpoint is invalid")]
    InvalidEndpoint,
    #[error("speed test is unavailable")]
    Unavailable,
    #[error("speed-test response is too large")]
    ResponseTooLarge,
}

pub async fn run_speed_test(
    disclosure: SpeedTestDisclosure,
) -> Result<SpeedTestResult, NetworkToolError> {
    const MAX_BYTES: u64 = 64 * 1024 * 1024;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("Stremio-Native/1")
        .build()
        .map_err(|_| NetworkToolError::Unavailable)?;
    let started = Instant::now();
    let mut response = client
        .get(disclosure.endpoint)
        .send()
        .await
        .map_err(|_| NetworkToolError::Unavailable)?;
    if !response.status().is_success() {
        return Err(NetworkToolError::Unavailable);
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BYTES)
    {
        return Err(NetworkToolError::ResponseTooLarge);
    }
    let mut bytes_received = 0_u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| NetworkToolError::Unavailable)?
    {
        bytes_received = bytes_received.saturating_add(chunk.len() as u64);
        if bytes_received > MAX_BYTES {
            return Err(NetworkToolError::ResponseTooLarge);
        }
    }
    let elapsed = started.elapsed().max(Duration::from_millis(1));
    Ok(SpeedTestResult {
        bytes_received,
        elapsed,
        bits_per_second: bytes_received as f64 * 8.0 / elapsed.as_secs_f64(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disclosure_rejects_non_tls_public_endpoints() {
        assert_eq!(
            SpeedTestDisclosure::accept("http://example.com/file"),
            Err(NetworkToolError::InvalidEndpoint)
        );
        assert!(SpeedTestDisclosure::accept("https://example.com/file").is_ok());
    }
}

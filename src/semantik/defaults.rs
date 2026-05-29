/// Production endpoint. Override via [`super::ClientBuilder::base_url`]
/// or the `NOETIVE_BASE_URL` environment variable.
pub(crate) const DEFAULT_BASE_URL: &str = "https://semantik.noetive.io";

/// Environment variable read by [`super::Client::from_env`] for the API key.
pub(crate) const ENV_API_KEY: &str = "NOETIVE_KEY_SECRET";

/// Environment variable read by [`super::Client::from_env`] for the base URL.
///
/// These two variables are the SDK's entire environment surface. The
/// targeting fields — `namespace`, `model`, and `dimensions` — are
/// deliberately NOT read from the environment and have no client-side
/// default: every publish, search, and subscribe must set them
/// explicitly. Defaulting `namespace` to a shared value would let a
/// caller who simply forgot the field route sensitive data into a
/// namespace they did not intend, so the SDK fails preflight instead.
pub(crate) const ENV_BASE_URL: &str = "NOETIVE_BASE_URL";

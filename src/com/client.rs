use crate::com::api::*;
use crate::utils::lock_or_recover;
use hostname::get;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Client as InnerClient,
};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use url::form_urlencoded::byte_serialize;
use url::Url;

/// A client for communicating with Pool/Proxy/Wallet.
#[derive(Clone, Debug)]
pub struct Client {
    inner: InnerClient,
    account_id_to_secret_phrase: Arc<HashMap<u64, String>>,
    base_uri: Url,
    proxy_details: ProxyDetails,
    headers: Arc<Mutex<HeaderMap>>,
}

/// Parameters used for nonce submission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmissionParameters {
    pub account_id: u64,
    pub nonce: u64,
    pub height: u64,
    pub block: u64,
    pub deadline_unadjusted: u64,
    pub deadline: u64,
    pub gen_sig: [u8; 32],
}

/// Useful for deciding which submission parameters are the newest and best.
impl Ord for SubmissionParameters {
    fn cmp(&self, other: &Self) -> Ordering {
        if self.block < other.block {
            Ordering::Less
        } else if self.block > other.block {
            Ordering::Greater
        } else if self.gen_sig == other.gen_sig {
            if self.deadline <= other.deadline {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        } else {
            Ordering::Less
        }
    }
}

impl PartialOrd for SubmissionParameters {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, PartialEq, Debug)]
pub enum ProxyDetails {
    Enabled,
    Disabled,
}

fn header_value(name: &str, value: &str) -> Result<HeaderValue, String> {
    value
        .parse()
        .map_err(|e| format!("invalid value for header '{}': {}", name, e))
}

impl Client {
    fn ua() -> String {
        format!("signum-miner/{}", env!("CARGO_PKG_VERSION"))
    }

    fn submit_nonce_headers(
        proxy_details: &ProxyDetails,
        total_size_gb: usize,
        additional_headers: &HashMap<String, String>,
    ) -> Result<HeaderMap, String> {
        let ua = Client::ua();
        let mut headers = HeaderMap::new();
        headers.insert("User-Agent", header_value("User-Agent", &ua)?);

        if *proxy_details == ProxyDetails::Enabled {
            headers.insert(
                "X-Capacity",
                header_value("X-Capacity", &total_size_gb.to_string())?,
            );
            headers.insert("X-Miner", header_value("X-Miner", &ua)?);

            let hostname = get()
                .ok()
                .and_then(|h| h.into_string().ok())
                .unwrap_or_default();

            headers.insert("X-Minername", header_value("X-Minername", &hostname)?);
            headers.insert(
                "X-Plotfile",
                header_value("X-Plotfile", &format!("signum-miner-proxy/{}", hostname))?,
            );
        }

        for (key, value) in additional_headers {
            let header_name = HeaderName::from_bytes(key.as_bytes()).map_err(|e| {
                format!("invalid header name '{}' in additional_headers: {}", key, e)
            })?;
            headers.insert(header_name, header_value(key, value)?);
        }

        Ok(headers)
    }

    pub fn new(
        base_uri: Url,
        mut secret_phrases: HashMap<u64, String>,
        timeout: u64,
        total_size_gb: usize,
        proxy_details: ProxyDetails,
        additional_headers: HashMap<String, String>,
    ) -> Result<Self, String> {
        for secret_phrase in secret_phrases.values_mut() {
            *secret_phrase = byte_serialize(secret_phrase.as_bytes()).collect();
        }

        let headers =
            Client::submit_nonce_headers(&proxy_details, total_size_gb, &additional_headers)?;

        let timeout = Duration::from_millis(timeout);
        let client = InnerClient::builder()
            .timeout(timeout)
            .connect_timeout(timeout)
            .pool_max_idle_per_host(2)
            .build()
            .map_err(|e| format!("failed to build HTTP client: {}", e))?;

        Ok(Self {
            inner: client,
            account_id_to_secret_phrase: Arc::new(secret_phrases),
            base_uri,
            proxy_details,
            headers: Arc::new(Mutex::new(headers)),
        })
    }

    pub fn uri_for(&self, path: &str) -> Url {
        let mut url = self.base_uri.clone();
        if let Ok(mut segments) = url.path_segments_mut() {
            segments.pop_if_empty().push(path);
        }
        url
    }

    pub fn update_capacity(&self, total_size_gb: usize) {
        if self.proxy_details == ProxyDetails::Enabled {
            if let Ok(value) = header_value("X-Capacity", &total_size_gb.to_string()) {
                lock_or_recover(&self.headers).insert("X-Capacity", value);
            }
        }
    }

    fn headers(&self) -> HeaderMap {
        lock_or_recover(&self.headers).clone()
    }

    pub async fn get_mining_info(&self) -> Result<MiningInfoResponse, FetchError> {
        let res = self
            .inner
            .get(self.uri_for("burst"))
            .headers(self.headers())
            .query(&GetMiningInfoRequest {
                request_type: "getMiningInfo",
            })
            .send()
            .await?
            .bytes()
            .await?;

        parse_json_result(&res).map_err(FetchError::from)
    }

    pub async fn submit_nonce(
        &self,
        submission_data: &SubmissionParameters,
    ) -> Result<SubmitNonceResponse, FetchError> {
        let empty = "".to_owned();
        let secret_phrase = self
            .account_id_to_secret_phrase
            .get(&submission_data.account_id)
            .unwrap_or(&empty);

        let mut query = format!(
            "requestType=submitNonce&accountId={}&nonce={}&secretPhrase={}&blockheight={}",
            submission_data.account_id,
            submission_data.nonce,
            secret_phrase,
            submission_data.height
        );

        if secret_phrase.is_empty() {
            query += &format!("&deadline={}", submission_data.deadline_unadjusted);
        }

        let mut headers = self.headers();
        if let Ok(value) = header_value("X-Deadline", &submission_data.deadline.to_string()) {
            headers.insert("X-Deadline", value);
        }

        let mut uri = self.uri_for("burst");
        uri.set_query(Some(&query));

        let res = self
            .inner
            .post(uri)
            .headers(headers)
            .send()
            .await?
            .bytes()
            .await?;

        parse_json_result(&res).map_err(FetchError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    static BASE_URL: &str = "https://europe.signum.network/";

    #[tokio::test]
    async fn test_submit_params_cmp() {
        let submit_params_1 = SubmissionParameters {
            account_id: 1337,
            nonce: 12,
            height: 112,
            block: 0,
            deadline_unadjusted: 7123,
            deadline: 1193,
            gen_sig: [0; 32],
        };

        let mut submit_params_2 = submit_params_1.clone();
        submit_params_2.block += 1;
        assert!(submit_params_1 < submit_params_2);

        let mut submit_params_2 = submit_params_1.clone();
        submit_params_2.deadline -= 1;
        assert!(submit_params_1 < submit_params_2);

        let mut submit_params_2 = submit_params_1.clone();
        submit_params_2.gen_sig[0] = 1;
        submit_params_2.deadline += 1;
        assert!(submit_params_1 < submit_params_2);

        let mut submit_params_2 = submit_params_1.clone();
        submit_params_2.deadline += 1;
        assert!(submit_params_1 > submit_params_2);
    }

    #[test]
    fn uri_for_appends_to_base_path() {
        let client = Client::new(
            Url::parse("http://localhost:8125").unwrap(),
            HashMap::new(),
            1000,
            1,
            ProxyDetails::Disabled,
            HashMap::new(),
        )
        .unwrap();
        assert_eq!(
            client.uri_for("burst").as_str(),
            "http://localhost:8125/burst"
        );

        let client = Client::new(
            Url::parse("http://proxy.example/prefix/").unwrap(),
            HashMap::new(),
            1000,
            1,
            ProxyDetails::Disabled,
            HashMap::new(),
        )
        .unwrap();
        assert_eq!(
            client.uri_for("burst").as_str(),
            "http://proxy.example/prefix/burst"
        );
    }

    #[test]
    fn invalid_additional_header_is_a_config_error() {
        let mut extra = HashMap::new();
        extra.insert("bad header".to_string(), "x".to_string());
        let err = Client::new(
            Url::parse("http://localhost:8125").unwrap(),
            HashMap::new(),
            1000,
            1,
            ProxyDetails::Disabled,
            extra,
        )
        .err()
        .unwrap();
        assert!(err.contains("bad header"), "{}", err);
    }

    #[test]
    fn proxy_headers_are_set_and_capacity_updates() {
        let client = Client::new(
            Url::parse("http://localhost:8125").unwrap(),
            HashMap::new(),
            1000,
            12,
            ProxyDetails::Enabled,
            HashMap::new(),
        )
        .unwrap();
        assert_eq!(client.headers()["X-Capacity"], "12");
        client.update_capacity(99);
        assert_eq!(client.headers()["X-Capacity"], "99");
        assert!(client.headers()["User-Agent"]
            .to_str()
            .unwrap()
            .starts_with("signum-miner/"));
    }

    /// Talks to a live pool; run with `cargo test -- --ignored` when online.
    #[tokio::test]
    #[ignore]
    async fn test_get_mining_info_and_submit_nonce() {
        let mut secret = HashMap::new();
        secret.insert(1337u64, "secret".to_owned());

        let client = Client::new(
            Url::parse(BASE_URL).unwrap(),
            secret,
            5000,
            12,
            ProxyDetails::Enabled,
            HashMap::new(),
        )
        .unwrap();

        let mining_info = client
            .get_mining_info()
            .await
            .expect("Failed to fetch mining info");

        let submission = SubmissionParameters {
            account_id: 1337,
            nonce: 12,
            height: mining_info.height,
            block: 1,
            deadline_unadjusted: 7123,
            deadline: 1193,
            gen_sig: [0; 32],
        };

        let result = client.submit_nonce(&submission).await;
        assert!(result.is_ok(), "submit_nonce failed: {:?}", result.err());
    }
}

use bytes::Bytes;
use serde::de::{self, DeserializeOwned};
use std::fmt;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetMiningInfoRequest<'a> {
    pub request_type: &'a str,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SubmitNonceResponse {
    #[serde(deserialize_with = "from_str_or_int")]
    pub deadline: u64,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct MiningInfoResponse {
    pub generation_signature: String,

    #[serde(deserialize_with = "from_str_or_int")]
    pub base_target: u64,

    #[serde(deserialize_with = "from_str_or_int")]
    pub height: u64,

    #[serde(
        default = "default_target_deadline",
        deserialize_with = "from_str_or_int"
    )]
    pub target_deadline: u64,
}

fn default_target_deadline() -> u64 {
    u64::MAX
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PoolErrorWrapper {
    error: PoolError,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolError {
    #[serde(default)]
    pub code: i32,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug)]
pub enum FetchError {
    Http(reqwest::Error),
    Pool(PoolError),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::Http(e) => write!(f, "http error: {}", e),
            FetchError::Pool(e) => {
                let msg: String = e.message.chars().take(200).collect();
                write!(f, "pool error {}: {}", e.code, msg.trim())
            }
        }
    }
}

impl From<reqwest::Error> for FetchError {
    fn from(err: reqwest::Error) -> FetchError {
        FetchError::Http(err)
    }
}

impl From<PoolError> for FetchError {
    fn from(err: PoolError) -> FetchError {
        FetchError::Pool(err)
    }
}

fn from_str_or_int<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: de::Deserializer<'de>,
{
    struct StringOrIntVisitor;

    impl<'de> de::Visitor<'de> for StringOrIntVisitor {
        type Value = u64;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("string or int")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            v.parse::<u64>().map_err(de::Error::custom)
        }

        fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
            Ok(v)
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
            u64::try_from(v).map_err(de::Error::custom)
        }
    }

    deserializer.deserialize_any(StringOrIntVisitor)
}

pub fn parse_json_result<T: DeserializeOwned>(body: &Bytes) -> Result<T, PoolError> {
    match serde_json::from_slice(body) {
        Ok(x) => Ok(x),
        _ => match serde_json::from_slice::<PoolErrorWrapper>(body) {
            Ok(x) => Err(x.error),
            _ => {
                let v = body.to_vec();
                Err(PoolError {
                    code: 0,
                    message: String::from_utf8_lossy(&v).to_string(),
                })
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mining_info_accepts_strings_and_ints() {
        let body = Bytes::from_static(
            br#"{"generationSignature":"ab","baseTarget":"123","height":456,"targetDeadline":"789"}"#,
        );
        let info: MiningInfoResponse = parse_json_result(&body).unwrap();
        assert_eq!(info.base_target, 123);
        assert_eq!(info.height, 456);
        assert_eq!(info.target_deadline, 789);

        let body = Bytes::from_static(br#"{"generationSignature":"ab","baseTarget":1,"height":2}"#);
        let info: MiningInfoResponse = parse_json_result(&body).unwrap();
        assert_eq!(info.target_deadline, u64::MAX);
    }

    #[test]
    fn pool_errors_are_decoded() {
        let body = Bytes::from_static(br#"{"error":{"code":1008,"message":"limit exceeded"}}"#);
        let err = parse_json_result::<SubmitNonceResponse>(&body).unwrap_err();
        assert_eq!(err.code, 1008);
        assert_eq!(err.message, "limit exceeded");

        let body = Bytes::from_static(b"<html>502 Bad Gateway</html>");
        let err = parse_json_result::<SubmitNonceResponse>(&body).unwrap_err();
        assert_eq!(err.code, 0);
        assert!(err.message.contains("502"));
    }

    #[test]
    fn submit_response_accepts_string_deadline() {
        let body = Bytes::from_static(br#"{"result":"success","deadline":"42"}"#);
        let res: SubmitNonceResponse = parse_json_result(&body).unwrap();
        assert_eq!(res.deadline, 42);
    }
}

use super::RestSettings;
use crate::{testing::node::RestError, wallet::Wallet};
use bech32::FromBase32;
use chain_core::property::Serialize;
use chain_crypto::PublicKey;
use chain_impl_mockchain::account;
use chain_impl_mockchain::fragment::Fragment;
use jortestkit::process::Wait;
use reqwest::{
    blocking::Response,
    header::{HeaderMap, HeaderValue, CONTENT_TYPE},
};
use std::fmt;

enum ApiVersion {
    V0,
    V1,
}

impl fmt::Display for ApiVersion {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ApiVersion::V0 => write!(f, "v0"),
            ApiVersion::V1 => write!(f, "v1"),
        }
    }
}

/// struct intends to return raw reqwest response
/// can be used to verify requests error codes or
/// to poll until data is available
#[derive(Debug, Clone)]
pub struct RawRest {
    uri: String,
    settings: RestSettings,
}

impl RawRest {
    pub fn new(uri: String, settings: RestSettings) -> Self {
        Self { uri, settings }
    }

    pub fn update_settings(&mut self, settings: RestSettings) {
        self.settings = settings;
    }

    pub fn epoch_reward_history(&self, epoch: u32) -> Result<Response, reqwest::Error> {
        let request = format!("rewards/epoch/{}", epoch);
        self.get(&request)
    }

    pub fn reward_history(&self, length: u32) -> Result<Response, reqwest::Error> {
        let request = format!("rewards/history/{}", length);
        self.get(&request)
    }

    fn print_request_path(&self, text: &str) {
        if self.settings.enable_debug {
            println!("Request: {}", text);
        }
    }

    fn get(&self, path: &str) -> Result<reqwest::blocking::Response, reqwest::Error> {
        let request = self.path(path);
        self.print_request_path(&request);
        match &self.settings.certificate {
            None => reqwest::blocking::get(&request),
            Some(cert) => {
                let client = reqwest::blocking::Client::builder()
                    .use_rustls_tls()
                    .add_root_certificate(cert.clone())
                    .build()
                    .unwrap();
                client.get(&request).send()
            }
        }
    }

    fn path(&self, path: &str) -> String {
        format!("{}/v0/{}", self.uri, path)
    }

    fn path_http_or_https(&self, path: &str, api_version: ApiVersion) -> String {
        if self.settings.use_https_for_post {
            let url = url::Url::parse(&self.uri).unwrap();
            return format!(
                "https://{}:443/{}/{}/{}",
                url.domain().unwrap(),
                url.path_segments().unwrap().next().unwrap(),
                api_version.to_string(),
                path
            );
        }
        format!("{}/{}/{}", self.uri, api_version, path)
    }

    pub fn stake_distribution(&self) -> Result<Response, reqwest::Error> {
        self.get("stake")
    }

    pub fn account_state(&self, wallet: &Wallet) -> Result<Response, reqwest::Error> {
        self.account_state_by_pk(&wallet.identifier().to_bech32_str())
    }

    pub fn account_state_by_pk(&self, bech32_str: &str) -> Result<Response, reqwest::Error> {
        let key = hex::encode(Self::try_from_str(bech32_str).as_ref().as_ref());
        self.get(&format!("account/{}", key))
    }

    fn try_from_str(src: &str) -> account::Identifier {
        let (_, data) = bech32::decode(src).unwrap();
        let dat = Vec::from_base32(&data).unwrap();
        let pk = PublicKey::from_binary(&dat).unwrap();
        account::Identifier::from(pk)
    }

    pub fn stake_pools(&self) -> Result<Response, reqwest::Error> {
        self.get("stake_pools")
    }

    pub fn stake_distribution_at(&self, epoch: u32) -> Result<Response, reqwest::Error> {
        let request = format!("stake/{}", epoch);
        self.get(&request)
    }

    pub fn stats(&self) -> Result<Response, reqwest::Error> {
        self.get("node/stats")
    }

    pub fn network_stats(&self) -> Result<Response, reqwest::Error> {
        self.get("network/stats")
    }

    pub fn p2p_quarantined(&self) -> Result<Response, reqwest::Error> {
        self.get("network/p2p/quarantined")
    }

    pub fn p2p_non_public(&self) -> Result<Response, reqwest::Error> {
        self.get("network/p2p/non_public")
    }

    pub fn p2p_available(&self) -> Result<Response, reqwest::Error> {
        self.get("network/p2p/available")
    }

    pub fn p2p_view(&self) -> Result<Response, reqwest::Error> {
        self.get("network/p2p/view")
    }

    pub fn leaders_log(&self) -> Result<Response, reqwest::Error> {
        self.get("leaders/logs")
    }

    pub fn tip(&self) -> Result<Response, reqwest::Error> {
        self.get("tip")
    }

    pub fn fragment_logs(&self) -> Result<Response, reqwest::Error> {
        self.get("fragment/logs")
    }

    pub fn leaders(&self) -> Result<Response, reqwest::Error> {
        self.get("leaders")
    }

    fn construct_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        headers
    }

    fn post(
        &self,
        path: &str,
        body: Vec<u8>,
    ) -> Result<reqwest::blocking::Response, reqwest::Error> {
        let builder = reqwest::blocking::Client::builder();
        let client = builder.build()?;
        client
            .post(&self.path_http_or_https(path, ApiVersion::V0))
            .headers(self.construct_headers())
            .body(body)
            .send()
    }

    pub fn send_fragment(&self, fragment: Fragment) -> Result<Response, reqwest::Error> {
        let raw = fragment.serialize_as_vec().unwrap();
        self.send_raw_fragment(raw)
    }

    pub fn send_raw_fragment(
        &self,
        body: Vec<u8>,
    ) -> Result<reqwest::blocking::Response, reqwest::Error> {
        self.post("message", body)
    }

    pub fn send_fragment_batch(
        &self,
        fragments: Vec<Fragment>,
    ) -> Result<Response, reqwest::Error> {
        let builder = reqwest::blocking::Client::builder();
        let client = builder.build()?;

        client
            .post(&self.path_http_or_https("fragments", ApiVersion::V1))
            .headers(self.construct_headers())
            .json(
                &fragments
                    .iter()
                    .map(|x| hex::encode(&x.serialize_as_vec().unwrap()))
                    .collect::<Vec<String>>(),
            )
            .send()
    }

    pub fn vote_plan_statuses(&self) -> Result<Response, reqwest::Error> {
        self.get("vote/active/plans")
    }

    pub fn send_until_ok<F>(&self, action: F, mut wait: Wait) -> Result<(), RestError>
    where
        F: Fn(&RawRest) -> Result<Response, reqwest::Error>,
    {
        loop {
            let response = action(&self);
            println!("Waiting for 200... {:?}", response);
            if let Ok(response) = response {
                if response.status().is_success() {
                    return Ok(());
                }
            }
            wait.check_timeout()?;
            wait.advance();
        }
    }
}

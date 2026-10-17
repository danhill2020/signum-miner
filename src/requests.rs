use crate::com::api::{FetchError, MiningInfoResponse};
use crate::com::client::{Client, ProxyDetails, SubmissionParameters};
use crate::future::prio_retry::PrioRetry;
use crate::metrics::SharedMetrics;
use crate::miner::Submission;
use crate::utils::write_or_recover;
use futures_util::stream::StreamExt;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use url::Url;

#[derive(Clone)]
pub struct RequestHandler {
    client: Client,
    tx_submit_data: mpsc::UnboundedSender<SubmissionParameters>,
}

impl RequestHandler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        base_uri: Url,
        secret_phrases: HashMap<u64, String>,
        timeout: u64,
        total_size_gb: usize,
        send_proxy_details: bool,
        additional_headers: HashMap<String, String>,
        metrics: SharedMetrics,
        handle: tokio::runtime::Handle,
    ) -> Result<RequestHandler, String> {
        let proxy_details = if send_proxy_details {
            ProxyDetails::Enabled
        } else {
            ProxyDetails::Disabled
        };

        let client = Client::new(
            base_uri,
            secret_phrases,
            timeout,
            total_size_gb,
            proxy_details,
            additional_headers,
        )?;

        let (tx_submit_data, rx_submit_nonce_data) = mpsc::unbounded_channel();
        RequestHandler::handle_submissions(
            client.clone(),
            rx_submit_nonce_data,
            tx_submit_data.clone(),
            metrics,
            handle,
        );

        Ok(RequestHandler {
            client,
            tx_submit_data,
        })
    }

    fn handle_submissions(
        client: Client,
        rx: mpsc::UnboundedReceiver<SubmissionParameters>,
        tx_submit_data: mpsc::UnboundedSender<SubmissionParameters>,
        metrics: SharedMetrics,
        handle: tokio::runtime::Handle,
    ) {
        handle.spawn(async move {
            let wrapped_rx = UnboundedReceiverStream::new(rx);
            let stream = PrioRetry::new(wrapped_rx, Duration::from_secs(3));

            let mut stream = Box::pin(stream);
            while let Some(submission_params) = stream.as_mut().next().await {
                let tx_submit_data = tx_submit_data.clone();
                let result = client.submit_nonce(&submission_params).await;

                match result {
                    Ok(res) => {
                        if submission_params.deadline != res.deadline {
                            log_deadline_mismatch(
                                submission_params.height,
                                submission_params.account_id,
                                submission_params.nonce,
                                submission_params.deadline,
                                res.deadline,
                            );
                            write_or_recover(&metrics).record_submission_failure();
                        } else {
                            log_submission_accepted(
                                submission_params.account_id,
                                submission_params.nonce,
                                submission_params.deadline,
                            );
                            write_or_recover(&metrics).record_submission_success(
                                submission_params.account_id,
                                submission_params.deadline,
                            );
                        }
                    }
                    Err(FetchError::Pool(e)) => {
                        if e.message.is_empty() || e.message == "limit exceeded" {
                            log_pool_busy(
                                submission_params.account_id,
                                submission_params.nonce,
                                submission_params.deadline,
                            );
                            if tx_submit_data.send(submission_params).is_err() {
                                error!("can't send submission params");
                            }
                        } else {
                            log_submission_not_accepted(
                                submission_params.height,
                                submission_params.account_id,
                                submission_params.nonce,
                                submission_params.deadline,
                                e.code,
                                &e.message,
                            );
                            write_or_recover(&metrics).record_submission_failure();
                        }
                    }
                    Err(FetchError::Http(x)) => {
                        log_submission_failed(
                            submission_params.account_id,
                            submission_params.nonce,
                            submission_params.deadline,
                            &x.to_string(),
                        );
                        write_or_recover(&metrics).record_network_error();
                        if tx_submit_data.send(submission_params).is_err() {
                            error!("can't send submission params");
                        }
                    }
                }
            }
        });
    }

    pub fn get_mining_info(
        &self,
    ) -> impl std::future::Future<Output = Result<MiningInfoResponse, FetchError>> + '_ {
        self.client.get_mining_info()
    }

    pub fn submit_nonce(&self, submission: Submission) {
        let res = self.tx_submit_data.send(SubmissionParameters {
            account_id: submission.account_id,
            nonce: submission.nonce,
            height: submission.height,
            block: submission.block,
            deadline_unadjusted: submission.deadline_unadjusted,
            deadline: submission.deadline,
            gen_sig: submission.gen_sig,
        });
        if let Err(e) = res {
            error!("can't send submission params: {}", e);
        }
    }

    pub fn update_capacity(&self, total_size_gb: usize) {
        self.client.update_capacity(total_size_gb);
    }
}

fn log_deadline_mismatch(
    height: u64,
    account_id: u64,
    nonce: u64,
    deadline: u64,
    deadline_pool: u64,
) {
    error!(
        "submit: deadlines mismatch, height={}, account={}, nonce={}, \
         deadline_miner={}, deadline_pool={}",
        height, account_id, nonce, deadline, deadline_pool
    );
}

fn log_submission_failed(account_id: u64, nonce: u64, deadline: u64, err: &str) {
    warn!(
        "{: <80}",
        format!(
            "submission failed, retrying: account={}, nonce={}, deadline={}, description={}",
            account_id, nonce, deadline, err
        )
    );
}

fn log_submission_not_accepted(
    height: u64,
    account_id: u64,
    nonce: u64,
    deadline: u64,
    err_code: i32,
    msg: &str,
) {
    error!(
        "submission not accepted: height={}, account={}, nonce={}, \
         deadline={}\n\tcode: {}\n\tmessage: {}",
        height, account_id, nonce, deadline, err_code, msg,
    );
}

fn log_submission_accepted(account_id: u64, nonce: u64, deadline: u64) {
    info!(
        "deadline accepted: account={}, nonce={}, deadline={}",
        account_id, nonce, deadline
    );
}

fn log_pool_busy(account_id: u64, nonce: u64, deadline: u64) {
    info!(
        "pool busy, retrying: account={}, nonce={}, deadline={}",
        account_id, nonce, deadline
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::new_shared_metrics;
    use std::collections::HashMap;
    use tokio::runtime::Runtime;

    #[test]
    fn submit_nonce_enqueues_without_a_live_server() {
        let rt = Runtime::new().expect("can't create runtime");
        let handle = rt.handle().clone();

        let request_handler = RequestHandler::new(
            Url::parse("http://127.0.0.1:9").unwrap(),
            HashMap::new(),
            300,
            12,
            true,
            HashMap::new(),
            new_shared_metrics(),
            handle,
        )
        .unwrap();

        request_handler.submit_nonce(Submission {
            account_id: 1337,
            nonce: 12,
            height: 111,
            block: 0,
            deadline_unadjusted: 7123,
            deadline: 1193,
            gen_sig: [0; 32],
        });
    }
}

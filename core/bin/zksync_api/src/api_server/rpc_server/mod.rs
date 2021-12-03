// Built-in uses
use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};

// External uses
use futures::{
    channel::{mpsc, oneshot},
    FutureExt, SinkExt, StreamExt,
};
use jsonrpc_core::{Error, IoHandler, MetaIoHandler, Metadata, Middleware, Params, Result};
use jsonrpc_http_server::{RequestMiddleware, RequestMiddlewareAction, ServerBuilder};

// Workspace uses

use zksync_storage::{
    chain::{
        block::records::StorageBlockDetails, operations::records::StoredExecutedPriorityOperation,
        operations_ext::records::TxReceiptResponse,
    },
    ConnectionPool, StorageProcessor,
};
use zksync_types::{tx::TxHash, Address, BlockNumber, TokenLike, TxFeeTypes};

// Local uses
use crate::{
    fee_ticker::{PriceError, ResponseBatchFee, ResponseFee, TickerRequest, TokenPriceRequestType},
    signature_checker::VerifySignatureRequest,
    utils::shared_lru_cache::AsyncLruCache,
};
use bigdecimal::BigDecimal;
use zksync_utils::panic_notify::{spawn_panic_handler, ThreadPanicNotify};

pub mod error;
mod rpc_impl;
mod rpc_trait;
pub mod types;

pub use self::rpc_trait::Rpc;
use self::types::*;
use super::tx_sender::TxSender;
use tokio::task::JoinHandle;
use zksync_config::configs::api::{CommonApiConfig, JsonRpcConfig};

const CLOUDFLARE_CONNECTING_IP_HEADER: &str = "CF-Connecting-IP";

#[derive(Clone)]
pub struct RpcApp {
    runtime_handle: tokio::runtime::Handle,

    cache_of_executed_priority_operations: AsyncLruCache<u32, StoredExecutedPriorityOperation>,
    cache_of_transaction_receipts: AsyncLruCache<Vec<u8>, TxReceiptResponse>,
    cache_of_complete_withdrawal_tx_hashes: AsyncLruCache<TxHash, String>,

    pub confirmations_for_eth_event: u64,

    tx_sender: TxSender,

    pub subsidized_ips: HashSet<String>,
}

impl RpcApp {
    pub fn new(
        connection_pool: ConnectionPool,
        sign_verify_request_sender: mpsc::Sender<VerifySignatureRequest>,
        ticker_request_sender: mpsc::Sender<TickerRequest>,
        config: &CommonApiConfig,
        private_url: String,
        confirmations_for_eth_event: u64,
    ) -> Self {
        let runtime_handle = tokio::runtime::Handle::try_current()
            .expect("RpcApp must be created from the context of Tokio Runtime");

        let api_requests_caches_size = config.caches_size;

        let tx_sender = TxSender::new(
            connection_pool,
            sign_verify_request_sender,
            ticker_request_sender,
            config,
            private_url,
        );

        RpcApp {
            runtime_handle,

            cache_of_executed_priority_operations: AsyncLruCache::new(api_requests_caches_size),
            cache_of_transaction_receipts: AsyncLruCache::new(api_requests_caches_size),
            cache_of_complete_withdrawal_tx_hashes: AsyncLruCache::new(api_requests_caches_size),

            confirmations_for_eth_event,

            tx_sender,

            subsidized_ips: config.subsidized_ips.clone().into_iter().collect(),
        }
    }

    pub fn extend<T: Metadata, S: Middleware<T>>(self, io: &mut MetaIoHandler<T, S>) {
        io.extend_with(self.to_delegate())
    }
}

struct MethodWithIpDescription {
    minimum_params: usize,
    // the last one is always the ip parameter
    maximum_params: usize,
}

impl MethodWithIpDescription {
    pub fn new(minimum_params: usize, maximum_params: usize) -> Self {
        MethodWithIpDescription {
            minimum_params,
            maximum_params,
        }
    }
}
struct IpInsertMiddleWare {}

// Appends `ip` as one of the call's parameters if needed
fn get_call_with_ip_if_needed(
    call: jsonrpc_core::MethodCall,
    ip: String,
) -> jsonrpc_core::MethodCall {
    let methods_with_ip = HashMap::from([
        ("tx_submit".to_owned(), MethodWithIpDescription::new(1, 4)),
        (
            "submit_txs_batch".to_owned(),
            MethodWithIpDescription::new(1, 3),
        ),
        ("get_tx_fee".to_owned(), MethodWithIpDescription::new(3, 4)),
        (
            "get_txs_batch_fee_in_wei".to_owned(),
            MethodWithIpDescription::new(3, 4),
        ),
    ]);

    let description = methods_with_ip.get(&call.method);

    let description = if let Some(description) = description {
        description
    } else {
        return call;
    };

    let mut new_call = call.clone();
    // We add ip only to array of parameters
    if let Params::Array(mut params) = call.params {
        // The query is definitely wrong. We may proceed and the server will handle it normally
        if params.len() > description.maximum_params || params.len() < description.minimum_params {
            return new_call;
        }

        // If the length is equsl to the maximum amount of the
        // maximum_params, then the user tried to override ip
        if params.len() == description.maximum_params {
            params.pop();
        }

        // Fill optional params with null
        while params.len() < description.maximum_params - 1 {
            params.push(serde_json::Value::Null);
        }

        params.push(serde_json::Value::String(ip));

        new_call.params = Params::Array(params);
        new_call
    } else {
        call
    }
}

async fn insert_ip(body: hyper::Body, ip: String) -> hyper::Result<Vec<u8>> {
    let body_stream: Vec<_> = body.collect().await;
    let body_bytes: hyper::Result<Vec<_>> = body_stream.into_iter().collect();

    // The `?` is here to let Rust resolve body_bytes as a vector of Bytes structs
    let mut body_bytes: Vec<u8> = body_bytes?
        .into_iter()
        .map(|b| b.into_iter().collect::<Vec<u8>>())
        .flatten()
        .collect();

    let body_str = String::from_utf8(body_bytes.clone());

    if let Ok(s) = body_str {
        let call: std::result::Result<jsonrpc_core::MethodCall, _> = serde_json::from_str(&s);
        if let Ok(call) = call {
            let new_call = get_call_with_ip_if_needed(call, ip);
            let new_body_bytes = serde_json::to_string(&new_call);
            if let Ok(s) = new_body_bytes {
                body_bytes = s.as_bytes().to_owned();
            }
        };
    }

    Ok(body_bytes)
}

impl RequestMiddleware for IpInsertMiddleWare {
    fn on_request(&self, request: hyper::Request<hyper::Body>) -> RequestMiddlewareAction {
        let (parts, body) = request.into_parts();

        let remote_ip = match parts.headers.get(CLOUDFLARE_CONNECTING_IP_HEADER) {
            Some(ip) => ip.to_str(),
            None => {
                return RequestMiddlewareAction::Proceed {
                    should_continue_on_invalid_cors: false,
                    request: hyper::Request::from_parts(parts, body),
                }
            }
        };
        let remote_ip = if let Err(e) = remote_ip {
            vlog::warn!("Failed to parse CF-Connecting-IP header. Reason: {}", e);
            return RequestMiddlewareAction::Proceed {
                should_continue_on_invalid_cors: false,
                request: hyper::Request::from_parts(parts, body),
            };
        } else {
            remote_ip.unwrap()
        };

        let body_bytes = insert_ip(body, remote_ip.to_owned()).into_stream();
        let body = hyper::Body::wrap_stream(body_bytes);

        RequestMiddlewareAction::Proceed {
            should_continue_on_invalid_cors: false,
            request: hyper::Request::from_parts(parts, body),
        }
    }
}

impl RpcApp {
    async fn access_storage(&self) -> Result<StorageProcessor<'_>> {
        self.tx_sender
            .pool
            .access_storage()
            .await
            .map_err(|_| Error::internal_error())
    }

    // cache access functions
    async fn get_executed_priority_operation(
        &self,
        serial_id: u32,
    ) -> Result<Option<StoredExecutedPriorityOperation>> {
        let start = Instant::now();
        let res = if let Some(executed_op) = self
            .cache_of_executed_priority_operations
            .get(&serial_id)
            .await
        {
            Some(executed_op)
        } else {
            let mut storage = self.access_storage().await?;
            let executed_op = storage
                .chain()
                .operations_schema()
                .get_executed_priority_operation(serial_id)
                .await
                .map_err(|err| {
                    vlog::warn!("Internal Server Error: '{}'; input: {}", err, serial_id);
                    Error::internal_error()
                })?;

            if let Some(executed_op) = executed_op.clone() {
                self.cache_of_executed_priority_operations
                    .insert(serial_id, executed_op)
                    .await;
            }

            executed_op
        };

        metrics::histogram!("api.rpc.get_executed_priority_operation", start.elapsed());
        Ok(res)
    }

    async fn get_block_info(&self, block_number: i64) -> Result<Option<StorageBlockDetails>> {
        let start = Instant::now();
        let res = self
            .tx_sender
            .blocks
            .get(&self.tx_sender.pool, BlockNumber(block_number as u32))
            .await
            .map_err(|_| Error::internal_error())?;
        metrics::histogram!("api.rpc.get_block_info", start.elapsed());
        Ok(res)
    }

    async fn get_tx_receipt(&self, tx_hash: TxHash) -> Result<Option<TxReceiptResponse>> {
        let start = Instant::now();
        let res = if let Some(tx_receipt) = self
            .cache_of_transaction_receipts
            .get(&tx_hash.as_ref().to_vec())
            .await
        {
            Some(tx_receipt)
        } else {
            let mut storage = self.access_storage().await?;
            let tx_receipt = storage
                .chain()
                .operations_ext_schema()
                .tx_receipt(tx_hash.as_ref())
                .await
                .map_err(|err| {
                    vlog::warn!(
                        "Internal Server Error: '{}'; input: {}",
                        err,
                        tx_hash.to_string()
                    );
                    Error::internal_error()
                })?;

            if let Some(tx_receipt) = tx_receipt.clone() {
                if tx_receipt.verified {
                    self.cache_of_transaction_receipts
                        .insert(tx_hash.as_ref().to_vec(), tx_receipt)
                        .await;
                }
            }

            tx_receipt
        };

        metrics::histogram!("api.rpc.get_tx_receipt", start.elapsed());
        Ok(res)
    }

    async fn token_allowed_for_fees(
        mut ticker_request_sender: mpsc::Sender<TickerRequest>,
        token: TokenLike,
    ) -> Result<bool> {
        let (sender, receiver) = oneshot::channel();
        ticker_request_sender
            .send(TickerRequest::IsTokenAllowed {
                token: token.clone(),
                response: sender,
            })
            .await
            .expect("ticker receiver dropped");
        receiver
            .await
            .expect("ticker answer sender dropped")
            .map_err(|err| {
                vlog::warn!("Internal Server Error: '{}'; input: {:?}", err, token);
                Error::internal_error()
            })
    }

    async fn ticker_batch_fee_request(
        mut ticker_request_sender: mpsc::Sender<TickerRequest>,
        transactions: Vec<(TxFeeTypes, Address)>,
        token: TokenLike,
    ) -> Result<ResponseBatchFee> {
        let req = oneshot::channel();
        ticker_request_sender
            .send(TickerRequest::GetBatchTxFee {
                transactions,
                token: token.clone(),
                response: req.0,
            })
            .await
            .expect("ticker receiver dropped");
        let resp = req.1.await.expect("ticker answer sender dropped");
        resp.map_err(|err| {
            vlog::warn!("Internal Server Error: '{}'; input: {:?}", err, token,);
            Error::internal_error()
        })
    }

    async fn ticker_request(
        mut ticker_request_sender: mpsc::Sender<TickerRequest>,
        tx_type: TxFeeTypes,
        address: Address,
        token: TokenLike,
    ) -> Result<ResponseFee> {
        let req = oneshot::channel();
        ticker_request_sender
            .send(TickerRequest::GetTxFee {
                tx_type,
                address,
                token: token.clone(),
                response: req.0,
            })
            .await
            .expect("ticker receiver dropped");
        let resp = req.1.await.expect("ticker answer sender dropped");
        resp.map_err(|err| {
            vlog::warn!(
                "Internal Server Error: '{}'; input: {:?}, {:?}",
                err,
                tx_type,
                token,
            );
            Error::internal_error()
        })
    }

    async fn ticker_price_request(
        mut ticker_request_sender: mpsc::Sender<TickerRequest>,
        token: TokenLike,
        req_type: TokenPriceRequestType,
    ) -> Result<BigDecimal> {
        let req = oneshot::channel();
        ticker_request_sender
            .send(TickerRequest::GetTokenPrice {
                token: token.clone(),
                response: req.0,
                req_type,
            })
            .await
            .expect("ticker receiver dropped");
        let resp = req.1.await.expect("ticker answer sender dropped");
        resp.map_err(|err| match err {
            PriceError::TokenNotFound(msg) => Error::invalid_params(msg),
            _ => {
                vlog::warn!("Internal Server Error: '{}'; input: {:?}", err, token);
                Error::internal_error()
            }
        })
    }

    async fn get_account_state(&self, address: Address) -> Result<AccountStateInfo> {
        let start = Instant::now();
        let mut storage = self.access_storage().await?;
        let account_info = storage
            .chain()
            .account_schema()
            .account_state_by_address(address)
            .await
            .map_err(|_| Error::internal_error())?;

        let mut result = AccountStateInfo {
            account_id: None,
            committed: Default::default(),
            verified: Default::default(),
        };

        if let Some((account_id, committed_state)) = account_info.committed {
            result.account_id = Some(account_id);
            result.committed = ResponseAccountState::try_restore(
                &mut storage,
                &self.tx_sender.tokens,
                committed_state,
            )
            .await?;
        };

        if let Some((_, verified_state)) = account_info.verified {
            result.verified = ResponseAccountState::try_restore(
                &mut storage,
                &self.tx_sender.tokens,
                verified_state,
            )
            .await?;
        };

        metrics::histogram!("api.rpc.get_account_state", start.elapsed());
        Ok(result)
    }

    async fn eth_tx_for_withdrawal(&self, withdrawal_hash: TxHash) -> Result<Option<String>> {
        let res = if let Some(complete_withdrawals_tx_hash) = self
            .cache_of_complete_withdrawal_tx_hashes
            .get(&withdrawal_hash)
            .await
        {
            Some(complete_withdrawals_tx_hash)
        } else {
            let mut storage = self.access_storage().await?;
            let complete_withdrawals_tx_hash = storage
                .chain()
                .operations_schema()
                .eth_tx_for_withdrawal(&withdrawal_hash)
                .await
                .map_err(|err| {
                    vlog::warn!(
                        "Internal Server Error: '{}'; input: {:?}",
                        err,
                        withdrawal_hash,
                    );
                    Error::internal_error()
                })?
                .map(|tx_hash| format!("0x{}", hex::encode(&tx_hash)));

            if let Some(complete_withdrawals_tx_hash) = complete_withdrawals_tx_hash.clone() {
                self.cache_of_complete_withdrawal_tx_hashes
                    .insert(withdrawal_hash, complete_withdrawals_tx_hash)
                    .await;
            }

            complete_withdrawals_tx_hash
        };
        Ok(res)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn start_rpc_server(
    connection_pool: ConnectionPool,
    sign_verify_request_sender: mpsc::Sender<VerifySignatureRequest>,
    ticker_request_sender: mpsc::Sender<TickerRequest>,
    config: &JsonRpcConfig,
    common_api_config: &CommonApiConfig,
    private_url: String,
    confirmations_for_eth_event: u64,
) -> JoinHandle<()> {
    let addr = config.http_bind_addr();
    let rpc_app = RpcApp::new(
        connection_pool,
        sign_verify_request_sender,
        ticker_request_sender,
        common_api_config,
        private_url,
        confirmations_for_eth_event,
    );

    let (handler, panic_sender) = spawn_panic_handler();
    std::thread::spawn(move || {
        let _panic_sentinel = ThreadPanicNotify(panic_sender);
        let mut io = IoHandler::new();
        rpc_app.extend(&mut io);

        let server = ServerBuilder::new(io)
            .threads(super::THREADS_PER_SERVER)
            .request_middleware(IpInsertMiddleWare {})
            .start_http(&addr)
            .unwrap();
        server.wait();
    });
    handler
}

#[cfg(test)]
mod test {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[test]
    fn tx_fee_type_serialization() {
        #[derive(Debug, Serialize, Deserialize, PartialEq)]
        struct Query {
            tx_type: TxFeeTypes,
        }

        let cases = vec![
            (
                Query {
                    tx_type: TxFeeTypes::Withdraw,
                },
                r#"{"tx_type":"Withdraw"}"#,
            ),
            (
                Query {
                    tx_type: TxFeeTypes::Transfer,
                },
                r#"{"tx_type":"Transfer"}"#,
            ),
        ];
        for (query, json_str) in cases {
            let ser = serde_json::to_string(&query).expect("ser");
            assert_eq!(ser, json_str);
            let de = serde_json::from_str::<Query>(&ser).expect("de");
            assert_eq!(query, de);
        }
    }
}

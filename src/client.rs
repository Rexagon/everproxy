use std::net::{IpAddr, SocketAddrV4};
use std::sync::{Arc, Weak};
use std::time::Duration;
use std::{convert::Infallible, net::SocketAddr};

use anyhow::{Context, Result};
use everscale_crypto::ed25519;
use everscale_jrpc_models::ContractStateResponse;
use everscale_jrpc_server::JrpcState;
use everscale_network::{adnl, dht, overlay, rldp, NetworkBuilder};
use futures::{StreamExt, TryStreamExt};
use global_config::GlobalConfig;
use hyper::server::conn::AddrStream;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server, StatusCode};

use nekoton::core::dens::Dens;
use nekoton::transport::models::{ExistingContract, RawContractState};
use nekoton::utils::SimpleClock;
use shared::*;
use ton_types::FxDashMap;

mod proxy_proto;
mod shared;

const DOMAIN: &str = ".ever";

struct BlockHandler {
    jrpc_state: Weak<JrpcState>,
}

#[async_trait::async_trait]
impl ton_indexer::Subscriber for BlockHandler {
    async fn process_block(&self, ctx: ton_indexer::ProcessBlockContext<'_>) -> Result<()> {
        let block_stuff = ctx.block_stuff();
        let Some(shard_state) = ctx.shard_state_stuff() else { return Ok(()) };

        // TODO: rewrite using base ton_indexer methods
        if let Some(jrpc_state) = self.jrpc_state.upgrade() {
            jrpc_state.handle_block(block_stuff, shard_state)?;
        }

        Ok(())
    }
}

struct LocalTransport {
    jrpc_state: Arc<JrpcState>,
}

#[async_trait::async_trait]
impl nekoton::transport::Transport for LocalTransport {
    fn info(&self) -> nekoton::transport::TransportInfo {
        nekoton::transport::TransportInfo {
            has_key_blocks: true,
            max_transactions_per_fetch: 1,
            reliable_behavior: nekoton::core::models::ReliableBehavior::IntensivePolling,
        }
    }

    async fn send_message(&self, message: &ton_block::Message) -> Result<()> {
        todo!()
    }

    async fn get_contract_state(
        &self,
        address: &ton_block::MsgAddressInt,
    ) -> Result<RawContractState> {
        let state = self.jrpc_state.get_contract_state(address)?;
        let state: everscale_jrpc_models::ContractStateResponse = serde_json::from_value(state)?;
        Ok(match state {
            everscale_jrpc_models::ContractStateResponse::Exists {
                account,
                timings,
                last_transaction_id,
            } => RawContractState::Exists(ExistingContract {
                account,
                timings,
                last_transaction_id,
            }),
            everscale_jrpc_models::ContractStateResponse::NotExists => RawContractState::NotExists,
        })
    }

    async fn get_accounts_by_code_hash(
        &self,
        code_hash: &ton_types::UInt256,
        limit: u8,
        continuation: &Option<ton_block::MsgAddressInt>,
    ) -> Result<Vec<ton_block::MsgAddressInt>> {
        todo!()
    }

    async fn get_transactions(
        &self,
        address: &ton_block::MsgAddressInt,
        from_lt: u64,
        count: u8,
    ) -> Result<Vec<nekoton::transport::models::RawTransaction>> {
        todo!()
    }

    async fn get_transaction(
        &self,
        id: &ton_types::UInt256,
    ) -> Result<Option<nekoton::transport::models::RawTransaction>> {
        todo!()
    }

    async fn get_dst_transaction(
        &self,
        message_hash: &ton_types::UInt256,
    ) -> Result<Option<nekoton::transport::models::RawTransaction>> {
        todo!()
    }

    async fn get_latest_key_block(&self) -> Result<ton_block::Block> {
        todo!()
    }

    async fn get_blockchain_config(
        &self,
        clock: &dyn nekoton::utils::Clock,
        force: bool,
    ) -> Result<ton_executor::BlockchainConfig> {
        todo!()
    }
}

struct Proxy {
    proxy_overlay: Arc<overlay::Overlay>,
    engine: Arc<ton_indexer::Engine>,
    dens: Dens,
}

impl Proxy {
    async fn new(port: u16) -> Result<Self> {
        tracing::info!("Started resolving");
        let my_addr = resolve_public_ip(port).await?;
        tracing::info!("Resolved public ip: {my_addr}");

        let adnl_options = adnl::NodeOptions {
            use_loopback_for_neighbours: true,
            ..Default::default()
        };

        let global_config = GlobalConfig::load("global-config.json")?;
        tracing::info!("Found {} static nodes", global_config.dht_nodes.len());

        let jrpc_state = Arc::new(JrpcState::default());
        let subscriber = Arc::new(BlockHandler {
            jrpc_state: Arc::downgrade(&jrpc_state),
        });

        let adnl_keys = ton_indexer::NodeKeys::generate();

        let engine = ton_indexer::Engine::new(
            ton_indexer::NodeConfig {
                ip_address: my_addr,
                adnl_keys,
                rocks_db_path: "./db/rocksdb".into(),
                file_db_path: "./db/files".into(),
                state_gc_options: None,
                blocks_gc_options: None,
                shard_state_cache_options: None,
                max_db_memory_usage: 2 << 40, // rand
                archive_options: None,
                sync_options: ton_indexer::SyncOptions {
                    old_blocks_policy: ton_indexer::OldBlocksPolicy::Ignore,
                    ..Default::default()
                },
                adnl_options,
                rldp_options: Default::default(),
                dht_options: Default::default(),
                overlay_shard_options: Default::default(),
                neighbours_options: Default::default(),
            },
            global_config,
            vec![subscriber],
        )
        .await?;

        engine.start().await?;

        let overlay_id = proxy_proto::compute_proxy_id(0);
        let (proxy_overlay, _) = engine
            .network()
            .overlay()
            .add_public_overlay(&overlay_id, overlay::OverlayOptions::default());

        let transport = Arc::new(LocalTransport { jrpc_state });

        tokio::time::sleep(Duration::from_secs(10)).await;
        let dens = Dens::builder(Arc::new(SimpleClock), transport)
            .register(
                &"0:a7d0694c025b61e1a4a846f1cf88980a5df8adf737d17ac58e35bf172c9fca29".parse()?,
            )
            .await?
            .build();

        Ok(Self {
            proxy_overlay,
            engine,
            dens,
        })
    }

    pub async fn handle(
        &self,
        client_ip: IpAddr,
        req: Request<Body>,
    ) -> Result<Response<Body>, Infallible> {
        let uri = req.uri().clone();
        // TODO: add support for multiple domains
        if let Some(host) = uri.host().unwrap_or_default().strip_suffix(DOMAIN) {
            Ok(match self.handle_domain(host, req).await {
                Ok(response) => response,
                Err(e) => Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::from(e.to_string()))
                    .unwrap(),
            })
        } else {
            match hyper_reverse_proxy::call(client_ip, &req.uri().to_string(), req).await {
                Ok(response) => Ok(response),
                Err(_error) => Ok(Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::empty())
                    .unwrap()),
            }
        }
    }

    pub async fn handle_domain(&self, host: &str, req: Request<Body>) -> Result<Response<Body>> {
        // TODO: req.body <=> Stream
        let body = hyper::body::to_bytes(req.into_body()).await?;

        // Rewrite using bidirectional stream
        let response = self.redirect(&host, body.as_ref()).await?;

        Ok(Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(response)) // TODO: stream body
            .unwrap())
    }

    pub async fn redirect(&self, host: &str, req: &[u8]) -> Result<Vec<u8>> {
        const TIMEOUT: u64 = 2; // sec

        // TODO: fallback to raw ADNL address if valid
        let peer_id = match self.dens.try_resolve(&format!("{host}{DOMAIN}"), 1).await? {
            nekoton::core::dens::ResolvedValue::Found(data) => {
                let addr = *ton_types::SliceData::from(data).get_next_hash()?.as_slice();
                adnl::NodeIdShort::new(addr)
            }
            _ => anyhow::bail!("Failed to resolve domain"),
        };
        tracing::info!("Resolved ADNL: {peer_id}");

        let dht = self.engine.network().dht();
        let adnl = self.engine.network().adnl();

        // TODO: add peers cache
        let (addr, peer_id_full) = dht.find_address(&peer_id).await?;
        adnl.add_peer(
            adnl::NewPeerContext::Dht,
            adnl.key_by_tag(2)?.id(),
            &peer_id,
            addr,
            peer_id_full,
        )?;

        // TODO: use RLDP query if query > 768 bytes
        let response = self
            .proxy_overlay
            .adnl_query(
                &adnl,
                &peer_id,
                proxy_proto::ProxyRequest { content: req },
                Some(TIMEOUT),
            )
            .await?
            .context("Timeout")?;

        let response: proxy_proto::ProxyResponse = tl_proto::deserialize(&response)?;

        Ok(response.content.to_owned())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let bind_addr = "127.0.0.1:8000";
    let addr: SocketAddr = bind_addr.parse().expect("Could not parse ip:port.");

    let proxy = Arc::new(Proxy::new(10000).await?);
    tracing::info!("Started proxy");

    let make_svc = make_service_fn(move |conn: &AddrStream| {
        let remote_addr = conn.remote_addr().ip();
        let proxy = proxy.clone();
        async move {
            Ok::<_, Infallible>(service_fn(move |req| {
                let proxy = proxy.clone();
                async move { proxy.handle(remote_addr, req).await }
            }))
        }
    });

    let server = Server::bind(&addr).serve(make_svc);

    println!("Running server on {:?}", addr);

    if let Err(e) = server.await {
        eprintln!("server error: {}", e);
    }

    Ok(())
}

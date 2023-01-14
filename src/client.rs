use std::net::{IpAddr, SocketAddrV4};
use std::sync::Arc;
use std::{convert::Infallible, net::SocketAddr};

use anyhow::{Context, Result};
use everscale_crypto::ed25519;
use everscale_network::{adnl, dht, overlay, rldp, NetworkBuilder};
use futures::{StreamExt, TryStreamExt};
use global_config::GlobalConfig;
use hyper::server::conn::AddrStream;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server, StatusCode};

use shared::*;

mod proxy_proto;
mod shared;

const DOMAIN: &str = ".ever";

struct Proxy {
    adnl: Arc<adnl::Node>,
    dht: Arc<dht::Node>,
    rldp: Arc<rldp::Node>,
    overlays: Arc<overlay::Node>,
    proxy_overlay: Arc<overlay::Overlay>,
}

impl Proxy {
    const KEY_TAG: usize = 1;

    async fn new(port: u16) -> Result<Self> {
        tracing::info!("Started resolving");
        let my_addr = resolve_public_ip(port).await?;
        tracing::info!("Resolved public ip: {my_addr}");

        let key = ed25519::SecretKey::generate(&mut rand::thread_rng());

        let keystore = adnl::Keystore::builder()
            .with_tagged_key(*key.as_bytes(), Self::KEY_TAG)?
            .build();

        let adnl_options = adnl::NodeOptions {
            use_loopback_for_neighbours: true,
            ..Default::default()
        };

        let dht_options = dht::NodeOptions::default();

        let (adnl, dht, rldp, overlays) =
            NetworkBuilder::with_adnl(my_addr, keystore, adnl_options)
                .with_dht(Self::KEY_TAG, dht_options)
                .with_rldp(rldp::NodeOptions::default())
                .with_overlay(Self::KEY_TAG)
                .build()?;

        let global_config = GlobalConfig::load("global-config.json")?;
        tracing::info!("Found {} static nodes", global_config.dht_nodes.len());

        for static_node in global_config.dht_nodes {
            dht.add_dht_peer(static_node).unwrap();
        }
        let new_nodes = dht.find_more_dht_nodes().await?;

        tracing::info!("Found {new_nodes} DHT nodes");

        let overlay_id = proxy_proto::compute_proxy_id(0);
        let (proxy_overlay, _) =
            overlays.add_public_overlay(&overlay_id, overlay::OverlayOptions::default());

        Ok(Self {
            adnl,
            dht,
            rldp,
            overlays,
            proxy_overlay,
        })
    }

    pub async fn handle(
        &self,
        client_ip: IpAddr,
        req: Request<Body>,
    ) -> Result<Response<Body>, Infallible> {
        let uri = req.uri().clone();
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
        let adnl_addr: Vec<u8> = hex::decode(host)?;
        let adnl_addr: [u8; 32] = adnl_addr.as_slice().try_into()?;

        let body = hyper::body::to_bytes(req.into_body()).await?;

        let response = self.redirect(&adnl_addr, body.as_ref()).await?;

        Ok(Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(response))
            .unwrap())
    }

    pub async fn redirect(&self, adnl_addr: &[u8; 32], req: &[u8]) -> Result<Vec<u8>> {
        const TIMEOUT: u64 = 2; // sec

        let peer_id = adnl::NodeIdShort::new(*adnl_addr);
        tracing::info!("Redirecting to {peer_id}");

        let (addr, peer_id_full) = self.dht.find_address(&peer_id).await?;
        self.adnl.add_peer(
            adnl::NewPeerContext::Dht,
            self.adnl.key_by_tag(Self::KEY_TAG)?.id(),
            &peer_id,
            addr,
            peer_id_full,
        )?;

        let response = self
            .proxy_overlay
            .adnl_query(
                &self.adnl,
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

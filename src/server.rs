use std::borrow::Cow;
use std::net::{IpAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use everscale_crypto::ed25519;
use everscale_network::{
    adnl, dht, overlay, rldp, NetworkBuilder, QueryConsumingResult, QuerySubscriber,
    SubscriberContext,
};

use global_config::GlobalConfig;

use shared::*;

mod proxy_proto;
mod shared;

struct Hoster {
    content: Vec<u8>,
}

#[async_trait::async_trait]
impl QuerySubscriber for Hoster {
    async fn try_consume_query<'a>(
        &self,
        _: SubscriberContext<'a>,
        constructor: u32,
        query: Cow<'a, [u8]>,
    ) -> Result<QueryConsumingResult<'a>> {
        match constructor {
            proxy_proto::ProxyRequest::TL_ID => Ok(QueryConsumingResult::Consumed(Some(
                tl_proto::serialize(proxy_proto::ProxyResponse {
                    content: &self.content,
                }),
            ))),
            _ => {
                tracing::warn!("Unknown constructor: {constructor:08x}");
                Ok(QueryConsumingResult::Rejected(query))
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    const DHT_KEY_TAG: usize = 1;
    const OVERLAY_KEY_TAG: usize = 2;

    let my_addr = resolve_public_ip(31000).await?;

    let dht_key = ed25519::SecretKey::generate(&mut rand::thread_rng());
    let overlay_key = ed25519::SecretKey::generate(&mut rand::thread_rng());

    let keystore = adnl::Keystore::builder()
        .with_tagged_key(*dht_key.as_bytes(), DHT_KEY_TAG)?
        .with_tagged_key(*overlay_key.as_bytes(), OVERLAY_KEY_TAG)?
        .build();

    let adnl_options = adnl::NodeOptions {
        use_loopback_for_neighbours: true,
        ..Default::default()
    };

    let dht_options = dht::NodeOptions::default();

    let (adnl, dht, _rldp, overlay) = NetworkBuilder::with_adnl(my_addr, keystore, adnl_options)
        .with_dht(DHT_KEY_TAG, dht_options)
        .with_rldp(rldp::NodeOptions::default())
        .with_overlay(OVERLAY_KEY_TAG)
        .build()?;

    let global_config = GlobalConfig::load("global-config.json")?;
    tracing::info!("Found {} static nodes", global_config.dht_nodes.len());

    for static_node in global_config.dht_nodes {
        dht.add_dht_peer(static_node).unwrap();
    }
    let new_nodes = dht.find_more_dht_nodes().await?;

    tracing::info!("Found {new_nodes} DHT nodes");

    let dht_key = adnl.key_by_tag(DHT_KEY_TAG)?;
    let overlay_key = adnl.key_by_tag(OVERLAY_KEY_TAG)?;

    tracing::info!(peer_id = %overlay_key.id());

    let hoster = Arc::new(Hoster {
        content: "Hello from ever".to_owned().into_bytes(),
    });

    let overlay_id = proxy_proto::compute_proxy_id(0);
    let (_ever_overlay, _) =
        overlay.add_public_overlay(&overlay_id, overlay::OverlayOptions::default());
    overlay.add_overlay_subscriber(overlay_id, hoster);

    let mut interval = tokio::time::interval(Duration::from_secs(10));
    loop {
        interval.tick().await;
        let stored = dht.store_address(overlay_key, my_addr).await?;
        tracing::info!("Stored ip: {stored}");
    }
}

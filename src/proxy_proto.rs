use everscale_network::overlay::IdShort;
use tl_proto::{TlRead, TlWrite};

#[derive(TlWrite, TlRead)]
#[tl(boxed, id = "proxyOverlayId", scheme = "scheme.tl")]
pub struct ProxyOverlayId {
    pub nonce: u32,
}

pub fn compute_proxy_id(nonce: u32) -> IdShort {
    IdShort::new(tl_proto::hash(ProxyOverlayId { nonce }))
}

#[derive(TlWrite, TlRead)]
#[tl(boxed, id = "proxy.request", scheme = "scheme.tl")]
pub struct ProxyRequest<'tl> {
    pub content: &'tl [u8],
}

#[derive(TlWrite, TlRead)]
#[tl(boxed, id = "proxy.response", scheme = "scheme.tl")]
pub struct ProxyResponse<'tl> {
    pub content: &'tl [u8],
}

proxy.request content:bytes = ProxyRequest;
proxy.response content:bytes = ProxyResponse;

proxyOverlayId nonce:int = ProxyOverlayId;

---functions---
proxy.redirect request:ProxyRequest = ProxyResponse;

#[macro_use]
mod macros;
mod future;
mod service;
mod types;

use std::{
    collections::HashMap,
    convert::TryInto,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    num::NonZeroU32,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

pub use future::Pending;
use http::{
    Request as HttpRequest, Response as HttpResponse,
    header::{HeaderMap, HeaderValue, USER_AGENT},
};
use service::{ClientConfig, ClientService};
use tower::{
    Layer, Service, ServiceBuilder, ServiceExt,
    retry::RetryLayer,
    util::{BoxCloneSyncService, BoxCloneSyncServiceLayer},
};
use types::{BoxedClientService, BoxedClientServiceLayer, GenericClientService, ResponseBody};
#[cfg(feature = "cookies")]
use {super::middleware::cookie::CookieManagerLayer, crate::cookie};

#[cfg(any(
    feature = "gzip",
    feature = "zstd",
    feature = "brotli",
    feature = "deflate",
))]
use super::middleware::decoder::{AcceptEncoding, DecompressionLayer};
#[cfg(feature = "websocket")]
use super::websocket::WebSocketRequestBuilder;
use super::{
    Body, EmulationProviderFactory,
    middleware::{
        redirect::FollowRedirectLayer,
        retry::Http2RetryPolicy,
        timeout::{ResponseBodyTimeoutLayer, TimeoutLayer},
    },
    request::{Request, RequestBuilder},
    response::Response,
};
#[cfg(feature = "hickory-dns")]
use crate::dns::hickory::{HickoryDnsResolver, LookupIpStrategy};
use crate::{
    IntoUrl, Method, OriginalHeaders, Proxy,
    connect::{BoxedConnectorLayer, BoxedConnectorService, Conn, Connector, Unnameable},
    core::{
        client::{Builder, Client as NativeClient, connect::TcpConnectOptions},
        ext::RequestConfig,
        rt::{TokioExecutor, tokio::TokioTimer},
    },
    dns::{DnsResolverWithOverrides, DynResolver, Resolve, gai::GaiResolver},
    error::{self, BoxError, Error},
    http1::Http1Config,
    http2::Http2Config,
    proxy::Matcher as ProxyMatcher,
    redirect::{self, RedirectPolicy},
    tls::{
        AlpnProtocol, CertStore, CertificateInput, Identity, KeyLogPolicy, TlsConfig, TlsVersion,
    },
};

/// An `Client` to make Requests with.
///
/// The Client has various configuration values to tweak, but the defaults
/// are set to what is usually the most commonly desired value. To configure a
/// `Client`, use `Client::builder()`.
///
/// The `Client` holds a connection pool internally, so it is advised that
/// you create one and **reuse** it.
///
/// You do **not** have to wrap the `Client` in an [`Rc`] or [`Arc`] to **reuse** it,
/// because it already uses an [`Arc`] internally.
///
/// [`Rc`]: std::rc::Rc
#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientRef>,
}

#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
enum ClientRef {
    Boxed(BoxedClientService),
    Generic(GenericClientService),
}

/// A `ClientBuilder` can be used to create a `Client` with custom configuration.
#[must_use]
pub struct ClientBuilder {
    config: Config,
}

/// The HTTP version preference for the client.
#[repr(u8)]
enum HttpVersionPref {
    Http1,
    Http2,
    All,
}

struct Config {
    error: Option<Error>,
    headers: HeaderMap,
    original_headers: Option<OriginalHeaders>,
    #[cfg(any(
        feature = "gzip",
        feature = "zstd",
        feature = "brotli",
        feature = "deflate",
    ))]
    accept_encoding: AcceptEncoding,
    connect_timeout: Option<Duration>,
    connection_verbose: bool,
    pool_idle_timeout: Option<Duration>,
    pool_max_idle_per_host: usize,
    pool_max_size: Option<NonZeroU32>,
    tcp_nodelay: bool,
    tcp_reuse_address: bool,
    tcp_keepalive: Option<Duration>,
    tcp_keepalive_interval: Option<Duration>,
    tcp_keepalive_retries: Option<u32>,
    tcp_connect_options: Option<TcpConnectOptions>,
    #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
    tcp_user_timeout: Option<Duration>,
    proxies: Vec<ProxyMatcher>,
    auto_sys_proxy: bool,
    redirect_policy: redirect::Policy,
    referer: bool,
    timeout: Option<Duration>,
    read_timeout: Option<Duration>,
    #[cfg(feature = "cookies")]
    cookie_store: Option<Arc<dyn cookie::CookieStore>>,
    #[cfg(feature = "hickory-dns")]
    hickory_dns: bool,
    dns_overrides: HashMap<String, Vec<SocketAddr>>,
    dns_resolver: Option<Arc<dyn Resolve>>,
    http_version_pref: HttpVersionPref,
    https_only: bool,
    http1_config: Http1Config,
    http2_config: Http2Config,
    http2_max_retry: usize,
    request_layers: Option<Vec<BoxedClientServiceLayer>>,
    connector_layers: Option<Vec<BoxedConnectorLayer>>,
    builder: Builder,
    tls_keylog_policy: Option<KeyLogPolicy>,
    tls_info: bool,
    tls_sni: bool,
    tls_verify_hostname: bool,
    tls_identity: Option<Identity>,
    tls_cert_store: CertStore,
    tls_cert_verification: bool,
    min_tls_version: Option<TlsVersion>,
    max_tls_version: Option<TlsVersion>,
    tls_config: TlsConfig,
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientBuilder {
    /// Constructs a new `ClientBuilder`.
    ///
    /// This is the same as `Client::builder()`.
    pub fn new() -> ClientBuilder {
        ClientBuilder {
            config: Config {
                error: None,
                headers: HeaderMap::new(),
                original_headers: None,
                #[cfg(any(
                    feature = "gzip",
                    feature = "zstd",
                    feature = "brotli",
                    feature = "deflate",
                ))]
                accept_encoding: AcceptEncoding::default(),
                connect_timeout: None,
                connection_verbose: false,
                pool_idle_timeout: Some(Duration::from_secs(90)),
                pool_max_idle_per_host: usize::MAX,
                pool_max_size: None,
                // TODO: Re-enable default duration once core's HttpConnector is fixed
                // to no longer error when an option fails.
                tcp_keepalive: None,
                tcp_keepalive_interval: None,
                tcp_keepalive_retries: None,
                tcp_connect_options: None,
                tcp_nodelay: true,
                tcp_reuse_address: false,
                #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
                tcp_user_timeout: None,
                proxies: Vec::new(),
                auto_sys_proxy: true,
                redirect_policy: redirect::Policy::default(),
                referer: true,
                timeout: None,
                read_timeout: None,
                #[cfg(feature = "hickory-dns")]
                hickory_dns: cfg!(feature = "hickory-dns"),
                #[cfg(feature = "cookies")]
                cookie_store: None,
                dns_overrides: HashMap::new(),
                dns_resolver: None,
                http_version_pref: HttpVersionPref::All,
                builder: NativeClient::builder(TokioExecutor::new()),
                https_only: false,
                http1_config: Http1Config::default(),
                http2_config: Http2Config::default(),
                http2_max_retry: 2,
                request_layers: None,
                connector_layers: None,
                tls_keylog_policy: None,
                tls_info: false,
                tls_sni: true,
                tls_verify_hostname: true,
                tls_identity: None,
                tls_cert_store: CertStore::default(),
                tls_cert_verification: true,
                min_tls_version: None,
                max_tls_version: None,
                tls_config: TlsConfig::default(),
            },
        }
    }

    /// Returns a `Client` that uses this `ClientBuilder` configuration.
    ///
    /// # Errors
    ///
    /// This method fails if a TLS backend cannot be initialized, or the resolver
    /// cannot load the system configuration.
    pub fn build(self) -> crate::Result<Client> {
        let mut config = self.config;

        if let Some(err) = config.error {
            return Err(err);
        }

        let mut proxies = config.proxies;
        if config.auto_sys_proxy {
            proxies.push(ProxyMatcher::system());
        }
        let proxies = Arc::new(proxies);
        let proxies_maybe_http_auth = proxies.iter().any(ProxyMatcher::maybe_has_http_auth);
        let proxies_maybe_http_custom_headers = proxies
            .iter()
            .any(ProxyMatcher::maybe_has_http_custom_headers);

        config
            .builder
            .http1_config(config.http1_config)
            .http2_config(config.http2_config)
            .http2_only(matches!(config.http_version_pref, HttpVersionPref::Http2))
            .http2_timer(TokioTimer::new())
            .pool_timer(TokioTimer::new())
            .pool_idle_timeout(config.pool_idle_timeout)
            .pool_max_idle_per_host(config.pool_max_idle_per_host)
            .pool_max_size(config.pool_max_size);

        let connector = {
            let resolver = {
                let mut resolver: Arc<dyn Resolve> = match config.dns_resolver {
                    Some(dns_resolver) => dns_resolver,
                    #[cfg(feature = "hickory-dns")]
                    None if config.hickory_dns => {
                        Arc::new(HickoryDnsResolver::new(LookupIpStrategy::Ipv4thenIpv6)?)
                    }
                    None => Arc::new(GaiResolver::new()),
                };

                if !config.dns_overrides.is_empty() {
                    resolver = Arc::new(DnsResolverWithOverrides::new(
                        resolver,
                        config.dns_overrides,
                    ));
                }
                DynResolver::new(resolver)
            };

            match config.http_version_pref {
                HttpVersionPref::Http1 => {
                    config.tls_config.alpn_protos = Some(AlpnProtocol::HTTP1.encode());
                }
                HttpVersionPref::Http2 => {
                    config.tls_config.alpn_protos = Some(AlpnProtocol::HTTP2.encode());
                }
                _ => {}
            }

            Connector::builder(proxies.clone(), resolver)
                .connect_timeout(config.connect_timeout)
                .tcp_keepalive(config.tcp_keepalive)
                .tcp_keepalive_interval(config.tcp_keepalive_interval)
                .tcp_keepalive_retries(config.tcp_keepalive_retries)
                .tcp_reuse_address(config.tcp_reuse_address)
                .tcp_connect_options(config.tcp_connect_options)
                .tcp_nodelay(config.tcp_nodelay)
                .verbose(config.connection_verbose)
                .tls_max_version(config.max_tls_version)
                .tls_min_version(config.min_tls_version)
                .tls_info(config.tls_info)
                .tls_sni(config.tls_sni)
                .tls_verify_hostname(config.tls_verify_hostname)
                .tls_cert_verification(config.tls_cert_verification)
                .tls_cert_store(config.tls_cert_store)
                .tls_identity(config.tls_identity)
                .tls_keylog_policy(config.tls_keylog_policy)
                .tcp_user_timeout(
                    #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
                    config.tcp_user_timeout,
                )
                .build(config.tls_config, config.connector_layers)?
        };

        let service = {
            let service = ClientService {
                client: config.builder.build(connector),
                config: Arc::new(ClientConfig {
                    default_headers: config.headers,
                    original_headers: RequestConfig::new(config.original_headers),
                    skip_default_headers: RequestConfig::default(),
                    https_only: config.https_only,
                    proxies,
                    proxies_maybe_http_auth,
                    proxies_maybe_http_custom_headers,
                }),
            };

            #[cfg(any(
                feature = "gzip",
                feature = "zstd",
                feature = "brotli",
                feature = "deflate",
            ))]
            let service = ServiceBuilder::new()
                .layer(DecompressionLayer::new(config.accept_encoding))
                .service(service);

            let service = ServiceBuilder::new()
                .layer(ResponseBodyTimeoutLayer::new(
                    config.timeout,
                    config.read_timeout,
                ))
                .service(service);

            #[cfg(feature = "cookies")]
            let service = ServiceBuilder::new()
                .layer(CookieManagerLayer::new(config.cookie_store))
                .service(service);

            let policy = RedirectPolicy::new(config.redirect_policy)
                .with_referer(config.referer)
                .with_https_only(config.https_only);

            let service = ServiceBuilder::new()
                .layer(FollowRedirectLayer::with_policy(policy))
                .service(service);

            let service = ServiceBuilder::new()
                .layer(RetryLayer::new(Http2RetryPolicy::new(
                    config.http2_max_retry,
                )))
                .service(service);

            match config.request_layers {
                Some(layers) => {
                    let service = layers.into_iter().fold(
                        BoxCloneSyncService::new(service),
                        |client_service, layer| {
                            ServiceBuilder::new().layer(layer).service(client_service)
                        },
                    );

                    let service = ServiceBuilder::new()
                        .layer(TimeoutLayer::new(config.timeout, config.read_timeout))
                        .service(service);

                    let service = ServiceBuilder::new()
                        .map_err(error::map_timeout_to_request_error)
                        .service(service);

                    ClientRef::Boxed(BoxCloneSyncService::new(service))
                }
                None => {
                    let service = ServiceBuilder::new()
                        .layer(TimeoutLayer::new(config.timeout, config.read_timeout))
                        .service(service);

                    let service = ServiceBuilder::new()
                        .map_err(error::map_timeout_to_request_error as _)
                        .service(service);

                    ClientRef::Generic(service)
                }
            }
        };

        Ok(Client {
            inner: Arc::new(service),
        })
    }

    // Higher-level options

    /// Sets the `User-Agent` header to be used by this client.
    ///
    /// # Example
    ///
    /// ```rust
    /// # async fn doc() -> wreq::Result<()> {
    /// // Name your user agent after your app?
    /// static APP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"),);
    ///
    /// let client = wreq::Client::builder().user_agent(APP_USER_AGENT).build()?;
    /// let res = client.get("https://www.rust-lang.org").send().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn user_agent<V>(mut self, value: V) -> ClientBuilder
    where
        V: TryInto<HeaderValue>,
        V::Error: Into<http::Error>,
    {
        match value.try_into() {
            Ok(value) => {
                self.config.headers.insert(USER_AGENT, value);
            }
            Err(err) => {
                self.config.error = Some(Error::builder(err.into()));
            }
        };
        self
    }

    /// Sets the default headers for every request.
    ///
    /// # Example
    ///
    /// ```rust
    /// use wreq::header;
    /// # async fn doc() -> wreq::Result<()> {
    /// let mut headers = header::HeaderMap::new();
    /// headers.insert("X-MY-HEADER", header::HeaderValue::from_static("value"));
    ///
    /// // Consider marking security-sensitive headers with `set_sensitive`.
    /// let mut auth_value = header::HeaderValue::from_static("secret");
    /// auth_value.set_sensitive(true);
    /// headers.insert(header::AUTHORIZATION, auth_value);
    ///
    /// // get a client builder
    /// let client = wreq::Client::builder().default_headers(headers).build()?;
    /// let res = client.get("https://www.rust-lang.org").send().await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Override the default headers:
    ///
    /// ```rust
    /// use wreq::header;
    /// # async fn doc() -> wreq::Result<()> {
    /// let mut headers = header::HeaderMap::new();
    /// headers.insert("X-MY-HEADER", header::HeaderValue::from_static("value"));
    ///
    /// // get a client builder
    /// let client = wreq::Client::builder().default_headers(headers).build()?;
    /// let res = client
    ///     .get("https://www.rust-lang.org")
    ///     .header("X-MY-HEADER", "new_value")
    ///     .send()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn default_headers(mut self, headers: HeaderMap) -> ClientBuilder {
        crate::util::replace_headers(&mut self.config.headers, headers);
        self
    }

    /// Sets the original headers for every request.
    pub fn original_headers(mut self, original_headers: OriginalHeaders) -> ClientBuilder {
        self.config.original_headers = Some(original_headers);
        self
    }

    /// Enable a persistent cookie store for the client.
    ///
    /// Cookies received in responses will be preserved and included in
    /// additional requests.
    ///
    /// By default, no cookie store is used.
    ///
    /// # Optional
    ///
    /// This requires the optional `cookies` feature to be enabled.
    #[cfg(feature = "cookies")]
    pub fn cookie_store(mut self, enable: bool) -> ClientBuilder {
        if enable {
            self.cookie_provider(Arc::new(cookie::Jar::default()))
        } else {
            self.config.cookie_store = None;
            self
        }
    }

    /// Set the persistent cookie store for the client.
    ///
    /// Cookies received in responses will be passed to this store, and
    /// additional requests will query this store for cookies.
    ///
    /// By default, no cookie store is used.
    ///
    /// # Optional
    ///
    /// This requires the optional `cookies` feature to be enabled.
    #[cfg(feature = "cookies")]
    pub fn cookie_provider<C: cookie::CookieStore + 'static>(
        mut self,
        cookie_store: Arc<C>,
    ) -> ClientBuilder {
        self.config.cookie_store = Some(cookie_store as _);
        self
    }

    /// Enable auto gzip decompression by checking the `Content-Encoding` response header.
    ///
    /// If auto gzip decompression is turned on:
    ///
    /// - When sending a request and if the request's headers do not already contain an
    ///   `Accept-Encoding` **and** `Range` values, the `Accept-Encoding` header is set to `gzip`.
    ///   The request body is **not** automatically compressed.
    /// - When receiving a response, if its headers contain a `Content-Encoding` value of `gzip`,
    ///   both `Content-Encoding` and `Content-Length` are removed from the headers' set. The
    ///   response body is automatically decompressed.
    ///
    /// If the `gzip` feature is turned on, the default option is enabled.
    ///
    /// # Optional
    ///
    /// This requires the optional `gzip` feature to be enabled
    #[cfg(feature = "gzip")]
    pub fn gzip(mut self, enable: bool) -> ClientBuilder {
        self.config.accept_encoding.gzip(enable);
        self
    }

    /// Enable auto brotli decompression by checking the `Content-Encoding` response header.
    ///
    /// If auto brotli decompression is turned on:
    ///
    /// - When sending a request and if the request's headers do not already contain an
    ///   `Accept-Encoding` **and** `Range` values, the `Accept-Encoding` header is set to `br`. The
    ///   request body is **not** automatically compressed.
    /// - When receiving a response, if its headers contain a `Content-Encoding` value of `br`, both
    ///   `Content-Encoding` and `Content-Length` are removed from the headers' set. The response
    ///   body is automatically decompressed.
    ///
    /// If the `brotli` feature is turned on, the default option is enabled.
    ///
    /// # Optional
    ///
    /// This requires the optional `brotli` feature to be enabled
    #[cfg(feature = "brotli")]
    pub fn brotli(mut self, enable: bool) -> ClientBuilder {
        self.config.accept_encoding.brotli(enable);
        self
    }

    /// Enable auto zstd decompression by checking the `Content-Encoding` response header.
    ///
    /// If auto zstd decompression is turned on:
    ///
    /// - When sending a request and if the request's headers do not already contain an
    ///   `Accept-Encoding` **and** `Range` values, the `Accept-Encoding` header is set to `zstd`.
    ///   The request body is **not** automatically compressed.
    /// - When receiving a response, if its headers contain a `Content-Encoding` value of `zstd`,
    ///   both `Content-Encoding` and `Content-Length` are removed from the headers' set. The
    ///   response body is automatically decompressed.
    ///
    /// If the `zstd` feature is turned on, the default option is enabled.
    ///
    /// # Optional
    ///
    /// This requires the optional `zstd` feature to be enabled
    #[cfg(feature = "zstd")]
    pub fn zstd(mut self, enable: bool) -> ClientBuilder {
        self.config.accept_encoding.zstd(enable);
        self
    }

    /// Enable auto deflate decompression by checking the `Content-Encoding` response header.
    ///
    /// If auto deflate decompression is turned on:
    ///
    /// - When sending a request and if the request's headers do not already contain an
    ///   `Accept-Encoding` **and** `Range` values, the `Accept-Encoding` header is set to
    ///   `deflate`. The request body is **not** automatically compressed.
    /// - When receiving a response, if it's headers contain a `Content-Encoding` value that equals
    ///   to `deflate`, both values `Content-Encoding` and `Content-Length` are removed from the
    ///   headers' set. The response body is automatically decompressed.
    ///
    /// If the `deflate` feature is turned on, the default option is enabled.
    ///
    /// # Optional
    ///
    /// This requires the optional `deflate` feature to be enabled
    #[cfg(feature = "deflate")]
    pub fn deflate(mut self, enable: bool) -> ClientBuilder {
        self.config.accept_encoding.deflate(enable);
        self
    }

    /// Disable auto response body zstd decompression.
    ///
    /// This method exists even if the optional `zstd` feature is not enabled.
    /// This can be used to ensure a `Client` doesn't use zstd decompression
    /// even if another dependency were to enable the optional `zstd` feature.
    pub fn no_zstd(self) -> ClientBuilder {
        #[cfg(feature = "zstd")]
        {
            self.zstd(false)
        }

        #[cfg(not(feature = "zstd"))]
        {
            self
        }
    }

    /// Disable auto response body gzip decompression.
    ///
    /// This method exists even if the optional `gzip` feature is not enabled.
    /// This can be used to ensure a `Client` doesn't use gzip decompression
    /// even if another dependency were to enable the optional `gzip` feature.
    pub fn no_gzip(self) -> ClientBuilder {
        #[cfg(feature = "gzip")]
        {
            self.gzip(false)
        }

        #[cfg(not(feature = "gzip"))]
        {
            self
        }
    }

    /// Disable auto response body brotli decompression.
    ///
    /// This method exists even if the optional `brotli` feature is not enabled.
    /// This can be used to ensure a `Client` doesn't use brotli decompression
    /// even if another dependency were to enable the optional `brotli` feature.
    pub fn no_brotli(self) -> ClientBuilder {
        #[cfg(feature = "brotli")]
        {
            self.brotli(false)
        }

        #[cfg(not(feature = "brotli"))]
        {
            self
        }
    }

    /// Disable auto response body deflate decompression.
    ///
    /// This method exists even if the optional `deflate` feature is not enabled.
    /// This can be used to ensure a `Client` doesn't use deflate decompression
    /// even if another dependency were to enable the optional `deflate` feature.
    pub fn no_deflate(self) -> ClientBuilder {
        #[cfg(feature = "deflate")]
        {
            self.deflate(false)
        }

        #[cfg(not(feature = "deflate"))]
        {
            self
        }
    }

    // Redirect options

    /// Set a `RedirectPolicy` for this client.
    ///
    /// Default will follow redirects up to a maximum of 10.
    pub fn redirect(mut self, policy: redirect::Policy) -> ClientBuilder {
        self.config.redirect_policy = policy;
        self
    }

    /// Enable or disable automatic setting of the `Referer` header.
    ///
    /// Default is `true`.
    pub fn referer(mut self, enable: bool) -> ClientBuilder {
        self.config.referer = enable;
        self
    }

    // Proxy options

    /// Add a `Proxy` to the list of proxies the `Client` will use.
    ///
    /// # Note
    ///
    /// Adding a proxy will disable the automatic usage of the "system" proxy.
    ///
    /// # Example
    /// ```
    /// use wreq::{
    ///     Client,
    ///     Proxy,
    /// };
    ///
    /// let proxy = Proxy::http("http://proxy:8080").unwrap();
    /// let client = Client::builder().proxy(proxy).build().unwrap();
    /// ```
    pub fn proxy(mut self, proxy: Proxy) -> ClientBuilder {
        self.config.proxies.push(proxy.into_matcher());
        self.config.auto_sys_proxy = false;
        self
    }

    /// Clear all `Proxies`, so `Client` will use no proxy anymore.
    ///
    /// # Note
    /// To add a proxy exclusion list, use [crate::proxy::Proxy::no_proxy()]
    /// on all desired proxies instead.
    ///
    /// This also disables the automatic usage of the "system" proxy.
    pub fn no_proxy(mut self) -> ClientBuilder {
        self.config.proxies.clear();
        self.config.auto_sys_proxy = false;
        self
    }

    // Timeout options

    /// Enables a request timeout.
    ///
    /// The timeout is applied from when the request starts connecting until the
    /// response body has finished.
    ///
    /// Default is no timeout.
    pub fn timeout(mut self, timeout: Duration) -> ClientBuilder {
        self.config.timeout = Some(timeout);
        self
    }

    /// Set a timeout for only the read phase of a `Client`.
    ///
    /// Default is `None`.
    pub fn read_timeout(mut self, timeout: Duration) -> ClientBuilder {
        self.config.read_timeout = Some(timeout);
        self
    }

    /// Set a timeout for only the connect phase of a `Client`.
    ///
    /// Default is `None`.
    ///
    /// # Note
    ///
    /// This **requires** the futures be executed in a tokio runtime with
    /// a tokio timer enabled.
    pub fn connect_timeout(mut self, timeout: Duration) -> ClientBuilder {
        self.config.connect_timeout = Some(timeout);
        self
    }

    /// Set whether connections should emit verbose logs.
    ///
    /// Enabling this option will emit [log][] messages at the `TRACE` level
    /// for read and write operations on connections.
    ///
    /// [log]: https://crates.io/crates/log
    pub fn connection_verbose(mut self, verbose: bool) -> ClientBuilder {
        self.config.connection_verbose = verbose;
        self
    }

    // HTTP options

    /// Set an optional timeout for idle sockets being kept-alive.
    ///
    /// Pass `None` to disable timeout.
    ///
    /// Default is 90 seconds.
    pub fn pool_idle_timeout<D>(mut self, val: D) -> ClientBuilder
    where
        D: Into<Option<Duration>>,
    {
        self.config.pool_idle_timeout = val.into();
        self
    }

    /// Sets the maximum idle connection per host allowed in the pool.
    pub fn pool_max_idle_per_host(mut self, max: usize) -> ClientBuilder {
        self.config.pool_max_idle_per_host = max;
        self
    }

    /// Sets the maximum number of connections in the pool.
    pub fn pool_max_size(mut self, max: u32) -> ClientBuilder {
        self.config.pool_max_size = NonZeroU32::new(max);
        self
    }

    /// Disable keep-alive for the client.
    pub fn no_keepalive(mut self) -> ClientBuilder {
        self.config.pool_max_idle_per_host = 0;
        self.config.tcp_keepalive = None;
        self
    }

    /// Only use HTTP/1.
    pub fn http1_only(mut self) -> ClientBuilder {
        self.config.http_version_pref = HttpVersionPref::Http1;
        self
    }

    /// Only use HTTP/2.
    pub fn http2_only(mut self) -> ClientBuilder {
        self.config.http_version_pref = HttpVersionPref::Http2;
        self
    }

    /// Sets the maximum number of safe retries for HTTP/2 connections.
    pub fn http2_max_retry(mut self, max: usize) -> ClientBuilder {
        self.config.http2_max_retry = max;
        self
    }

    // TCP options

    /// Set whether sockets have `TCP_NODELAY` enabled.
    ///
    /// Default is `true`.
    pub fn tcp_nodelay(mut self, enabled: bool) -> ClientBuilder {
        self.config.tcp_nodelay = enabled;
        self
    }

    /// Set that all sockets have `SO_KEEPALIVE` set with the supplied duration.
    ///
    /// If `None`, the option will not be set.
    pub fn tcp_keepalive<D>(mut self, val: D) -> ClientBuilder
    where
        D: Into<Option<Duration>>,
    {
        self.config.tcp_keepalive = val.into();
        self
    }

    /// Set that all sockets have `SO_KEEPALIVE` set with the supplied interval.
    ///
    /// If `None`, the option will not be set.
    pub fn tcp_keepalive_interval<D>(mut self, val: D) -> ClientBuilder
    where
        D: Into<Option<Duration>>,
    {
        self.config.tcp_keepalive_interval = val.into();
        self
    }

    /// Set that all sockets have `SO_KEEPALIVE` set with the supplied retry count.
    ///
    /// If `None`, the option will not be set.
    pub fn tcp_keepalive_retries<C>(mut self, retries: C) -> ClientBuilder
    where
        C: Into<Option<u32>>,
    {
        self.config.tcp_keepalive_retries = retries.into();
        self
    }

    /// Set that all sockets have `TCP_USER_TIMEOUT` set with the supplied duration.
    ///
    /// This option controls how long transmitted data may remain unacknowledged before
    /// the connection is force-closed.
    ///
    /// The current default is `None` (option disabled).
    #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
    pub fn tcp_user_timeout<D>(mut self, val: D) -> ClientBuilder
    where
        D: Into<Option<Duration>>,
    {
        self.config.tcp_user_timeout = val.into();
        self
    }

    /// Set whether sockets have `SO_REUSEADDR` enabled.
    pub fn tcp_reuse_address(mut self, enabled: bool) -> ClientBuilder {
        self.config.tcp_reuse_address = enabled;
        self
    }

    /// Bind to a local IP Address.
    ///
    /// # Example
    ///
    /// ```
    /// use std::net::IpAddr;
    /// let local_addr = IpAddr::from([12, 4, 1, 8]);
    /// let client = wreq::Client::builder()
    ///     .local_address(local_addr)
    ///     .build()
    ///     .unwrap();
    /// ```
    pub fn local_address<T>(mut self, addr: T) -> ClientBuilder
    where
        T: Into<Option<IpAddr>>,
    {
        self.config
            .tcp_connect_options
            .get_or_insert_default()
            .set_local_address(addr.into());
        self
    }

    /// Set that all sockets are bound to the configured IPv4 or IPv6 address (depending on host's
    /// preferences) before connection.
    pub fn local_addresses<V4, V6>(mut self, ipv4: V4, ipv6: V6) -> ClientBuilder
    where
        V4: Into<Option<Ipv4Addr>>,
        V6: Into<Option<Ipv6Addr>>,
    {
        self.config
            .tcp_connect_options
            .get_or_insert_default()
            .set_local_addresses(ipv4.into(), ipv6.into());
        self
    }

    /// Bind to an interface by `SO_BINDTODEVICE`.
    ///
    /// # Example
    ///
    /// ```
    /// let interface = "lo";
    /// let client = wreq::Client::builder()
    ///     .interface(interface)
    ///     .build()
    ///     .unwrap();
    /// ```
    #[cfg(any(
        target_os = "android",
        target_os = "fuchsia",
        target_os = "illumos",
        target_os = "ios",
        target_os = "linux",
        target_os = "macos",
        target_os = "solaris",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos",
    ))]
    pub fn interface<T>(mut self, interface: T) -> ClientBuilder
    where
        T: Into<std::borrow::Cow<'static, str>>,
    {
        self.config
            .tcp_connect_options
            .get_or_insert_default()
            .set_interface(interface.into());
        self
    }

    // TLS/HTTP2 emulation options

    /// Configures the client builder to emulation the specified HTTP context.
    ///
    /// This method sets the necessary headers, HTTP/1 and HTTP/2 configurations, and TLS config
    /// to use the specified HTTP context. It allows the client to mimic the behavior of different
    /// versions or setups, which can be useful for testing or ensuring compatibility with various
    /// environments.
    ///
    /// # Note
    /// This will overwrite the existing configuration.
    /// You must set emulation before you can perform subsequent HTTP1/HTTP2/TLS fine-tuning.
    ///
    /// # Example
    ///
    /// ```rust
    /// use wreq::{
    ///     Client,
    ///     Emulation,
    /// };
    /// use wreq_util::Emulation;
    ///
    /// let client = Client::builder()
    ///     .emulation(Emulation::Firefox128)
    ///     .build()
    ///     .unwrap();
    /// ```
    pub fn emulation<P>(mut self, factory: P) -> ClientBuilder
    where
        P: EmulationProviderFactory,
    {
        use std::mem::swap;

        let mut emulation = factory.emulation();

        if let Some(mut headers) = emulation.default_headers {
            swap(&mut self.config.headers, &mut headers);
        }

        if emulation.original_headers.is_some() {
            swap(
                &mut self.config.original_headers,
                &mut emulation.original_headers,
            );
        }

        if let Some(mut http1_config) = emulation.http1_config.take() {
            swap(&mut self.config.http1_config, &mut http1_config);
        }

        if let Some(mut http2_config) = emulation.http2_config.take() {
            swap(&mut self.config.http2_config, &mut http2_config);
        }

        if let Some(mut tls_config) = emulation.tls_config.take() {
            swap(&mut self.config.tls_config, &mut tls_config);
        }

        self
    }

    /// Configures SSL/TLS certificate pinning for the client.
    ///
    /// This method allows you to specify a set of PEM-encoded certificates that the client
    /// will pin to, ensuring that only these certificates are trusted during SSL/TLS connections.
    /// This provides an additional layer of security by preventing man-in-the-middle (MITM)
    /// attacks, even if a malicious certificate is issued by a trusted Certificate Authority
    /// (CA).
    ///
    /// # Parameters
    ///
    /// - `certs`: An iterator of DER-encoded certificates. Each certificate should be provided as a
    ///   byte slice (`&[u8]`).
    pub fn ssl_pinning<'c, I>(mut self, certs: I) -> ClientBuilder
    where
        I: IntoIterator,
        I::Item: Into<CertificateInput<'c>>,
    {
        match CertStore::from_der_certs(certs) {
            Ok(store) => {
                self.config.tls_cert_store = store;
            }
            Err(err) => self.config.error = Some(err),
        }
        self
    }

    /// Sets the identity to be used for client certificate authentication.
    pub fn identity(mut self, identity: Identity) -> ClientBuilder {
        self.config.tls_identity = Some(identity);
        self
    }

    /// Controls the use of certificate validation.
    ///
    /// Defaults to `true`.
    ///
    /// # Warning
    ///
    /// You should think very carefully before using this method. If
    /// invalid certificates are trusted, *any* certificate for *any* site
    /// will be trusted for use. This includes expired certificates. This
    /// introduces significant vulnerabilities, and should only be used
    /// as a last resort.
    pub fn cert_verification(mut self, cert_verification: bool) -> ClientBuilder {
        self.config.tls_cert_verification = cert_verification;
        self
    }

    /// Sets the verify certificate store for the client.
    ///
    /// This method allows you to specify a custom verify certificate store to be used
    /// for TLS connections. By default, the system's verify certificate store is used.
    ///
    /// # Parameters
    ///
    /// - `store`: The verify certificate store to use. This can be a custom implementation of the
    ///   `IntoCertStore` trait or one of the predefined options.
    ///
    /// # Notes
    ///
    /// - Using a custom verify certificate store can be useful in scenarios where you need to trust
    ///   specific certificates that are not included in the system's default store.
    /// - Ensure that the provided verify certificate store is properly configured to avoid
    ///   potential security risks.
    pub fn cert_store(mut self, store: CertStore) -> ClientBuilder {
        self.config.tls_cert_store = store;
        self
    }

    /// Configures the use of Server Name Indication (SNI) when connecting.
    ///
    /// Defaults to `true`.
    pub fn tls_sni(mut self, tls_sni: bool) -> ClientBuilder {
        self.config.tls_sni = tls_sni;
        self
    }

    /// Configures TLS key logging policy for the client.
    pub fn keylog(mut self, policy: KeyLogPolicy) -> ClientBuilder {
        self.config.tls_keylog_policy = Some(policy);
        self
    }

    /// Configures the use of hostname verification when connecting.
    ///
    /// Defaults to `true`.
    /// # Warning
    ///
    /// You should think very carefully before you use this method. If hostname verification is not
    /// used, *any* valid certificate for *any* site will be trusted for use from any other. This
    /// introduces a significant vulnerability to man-in-the-middle attacks.
    pub fn verify_hostname(mut self, verify_hostname: bool) -> ClientBuilder {
        self.config.tls_verify_hostname = verify_hostname;
        self
    }

    /// Set the minimum required TLS version for connections.
    ///
    /// By default the TLS backend's own default is used.
    pub fn min_tls_version(mut self, version: TlsVersion) -> ClientBuilder {
        self.config.min_tls_version = Some(version);
        self
    }

    /// Set the maximum allowed TLS version for connections.
    ///
    /// By default there's no maximum.
    pub fn max_tls_version(mut self, version: TlsVersion) -> ClientBuilder {
        self.config.max_tls_version = Some(version);
        self
    }

    /// Add TLS information as `TlsInfo` extension to responses.
    ///
    /// # Optional
    ///
    /// feature to be enabled.
    pub fn tls_info(mut self, tls_info: bool) -> ClientBuilder {
        self.config.tls_info = tls_info;
        self
    }

    /// Restrict the Client to be used with HTTPS only requests.
    ///
    /// Defaults to false.
    pub fn https_only(mut self, enabled: bool) -> ClientBuilder {
        self.config.https_only = enabled;
        self
    }

    // DNS options

    /// Disables the hickory-dns async resolver.
    ///
    /// This method exists even if the optional `hickory-dns` feature is not enabled.
    /// This can be used to ensure a `Client` doesn't use the hickory-dns async resolver
    /// even if another dependency were to enable the optional `hickory-dns` feature.
    #[cfg(feature = "hickory-dns")]
    pub fn no_hickory_dns(mut self) -> ClientBuilder {
        self.config.hickory_dns = false;
        self
    }

    /// Override DNS resolution for specific domains to a particular IP address.
    ///
    /// Warning
    ///
    /// Since the DNS protocol has no notion of ports, if you wish to send
    /// traffic to a particular port you must include this port in the URL
    /// itself, any port in the overridden addr will be ignored and traffic sent
    /// to the conventional port for the given scheme (e.g. 80 for http).
    pub fn resolve(self, domain: &str, addr: SocketAddr) -> ClientBuilder {
        self.resolve_to_addrs(domain, &[addr])
    }

    /// Override DNS resolution for specific domains to particular IP addresses.
    ///
    /// Warning
    ///
    /// Since the DNS protocol has no notion of ports, if you wish to send
    /// traffic to a particular port you must include this port in the URL
    /// itself, any port in the overridden addresses will be ignored and traffic sent
    /// to the conventional port for the given scheme (e.g. 80 for http).
    pub fn resolve_to_addrs(mut self, domain: &str, addrs: &[SocketAddr]) -> ClientBuilder {
        self.config
            .dns_overrides
            .insert(domain.to_string(), addrs.to_vec());
        self
    }

    /// Override the DNS resolver implementation.
    ///
    /// Pass an `Arc` wrapping a trait object implementing `Resolve`.
    /// Overrides for specific names passed to `resolve` and `resolve_to_addrs` will
    /// still be applied on top of this resolver.
    pub fn dns_resolver<R: Resolve + 'static>(mut self, resolver: Arc<R>) -> ClientBuilder {
        self.config.dns_resolver = Some(resolver as _);
        self
    }

    /// Adds a new Tower [`Layer`](https://docs.rs/tower/latest/tower/trait.Layer.html) to the
    /// request [`Service`](https://docs.rs/tower/latest/tower/trait.Service.html) which is responsible
    /// for request processing.
    ///
    /// Each subsequent invocation of this function will wrap previous layers.
    ///
    /// If configured, the `timeout` will be the outermost layer.
    ///
    /// Example usage:
    /// ```
    /// use std::time::Duration;
    ///
    /// let client = wreq::Client::builder()
    ///     .timeout(Duration::from_millis(200))
    ///     .layer(tower::timeout::TimeoutLayer::new(Duration::from_millis(50)))
    ///     .build()
    ///     .unwrap();
    /// ```
    pub fn layer<L>(mut self, layer: L) -> ClientBuilder
    where
        L: Layer<BoxedClientService> + Clone + Send + Sync + 'static,
        L::Service: Service<HttpRequest<Body>, Response = HttpResponse<ResponseBody>, Error = BoxError>
            + Clone
            + Send
            + Sync
            + 'static,
        <L::Service as Service<HttpRequest<Body>>>::Future: Send + 'static,
    {
        let layer = BoxCloneSyncServiceLayer::new(layer);
        self.config
            .request_layers
            .get_or_insert_default()
            .push(layer);
        self
    }

    /// Adds a new Tower [`Layer`](https://docs.rs/tower/latest/tower/trait.Layer.html) to the
    /// base connector [`Service`](https://docs.rs/tower/latest/tower/trait.Service.html) which
    /// is responsible for connection establishment.a
    ///
    /// Each subsequent invocation of this function will wrap previous layers.
    ///
    /// If configured, the `connect_timeout` will be the outermost layer.
    ///
    /// Example usage:
    /// ```
    /// use std::time::Duration;
    ///
    /// let client = wreq::Client::builder()
    ///     // resolved to outermost layer, meaning while we are waiting on concurrency limit
    ///     .connect_timeout(Duration::from_millis(200))
    ///     // underneath the concurrency check, so only after concurrency limit lets us through
    ///     .connector_layer(tower::timeout::TimeoutLayer::new(Duration::from_millis(50)))
    ///     .connector_layer(tower::limit::concurrency::ConcurrencyLimitLayer::new(2))
    ///     .build()
    ///     .unwrap();
    /// ```
    pub fn connector_layer<L>(mut self, layer: L) -> ClientBuilder
    where
        L: Layer<BoxedConnectorService> + Clone + Send + Sync + 'static,
        L::Service:
            Service<Unnameable, Response = Conn, Error = BoxError> + Clone + Send + Sync + 'static,
        <L::Service as Service<Unnameable>>::Future: Send + 'static,
    {
        let layer = BoxCloneSyncServiceLayer::new(layer);
        self.config
            .connector_layers
            .get_or_insert_default()
            .push(layer);
        self
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    /// Constructs a new `Client`.
    ///
    /// # Panics
    ///
    /// This method panics if a TLS backend cannot be initialized, or the resolver
    /// cannot load the system configuration.
    ///
    /// Use `Client::builder()` if you wish to handle the failure as an `Error`
    /// instead of panicking.
    pub fn new() -> Client {
        ClientBuilder::new().build().expect("Client::new()")
    }

    /// Create a `ClientBuilder` specifically configured for WebSocket connections.
    ///
    /// This method configures the `ClientBuilder` to use HTTP/1.0 only, which is required for
    /// certain WebSocket connections.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    /// Convenience method to make a `GET` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever the supplied `Url` cannot be parsed.
    pub fn get<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::GET, url)
    }

    /// Upgrades the [`RequestBuilder`] to perform a
    /// websocket handshake. This returns a wrapped type, so you must do
    /// this after you set up your request, and just before you send the
    /// request.
    #[cfg(feature = "websocket")]
    pub fn websocket<U: IntoUrl>(&self, url: U) -> WebSocketRequestBuilder {
        WebSocketRequestBuilder::new(self.request(Method::GET, url))
    }

    /// Convenience method to make a `POST` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever the supplied `Url` cannot be parsed.
    pub fn post<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::POST, url)
    }

    /// Convenience method to make a `PUT` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever the supplied `Url` cannot be parsed.
    pub fn put<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::PUT, url)
    }

    /// Convenience method to make a `PATCH` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever the supplied `Url` cannot be parsed.
    pub fn patch<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::PATCH, url)
    }

    /// Convenience method to make a `DELETE` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever the supplied `Url` cannot be parsed.
    pub fn delete<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::DELETE, url)
    }

    /// Convenience method to make a `HEAD` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever the supplied `Url` cannot be parsed.
    pub fn head<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::HEAD, url)
    }

    /// Start building a `Request` with the `Method` and `Url`.
    ///
    /// Returns a `RequestBuilder`, which will allow setting headers and
    /// the request body before sending.
    ///
    /// # Errors
    ///
    /// This method fails whenever the supplied `Url` cannot be parsed.
    pub fn request<U: IntoUrl>(&self, method: Method, url: U) -> RequestBuilder {
        let req = url.into_url().map(move |url| Request::new(method, url));
        RequestBuilder::new(self.clone(), req)
    }

    /// Executes a `Request`.
    ///
    /// A `Request` can be built manually with `Request::new()` or obtained
    /// from a RequestBuilder with `RequestBuilder::build()`.
    ///
    /// You should prefer to use the `RequestBuilder` and
    /// `RequestBuilder::send()`.
    ///
    /// # Errors
    ///
    /// This method fails if there was an error while sending request,
    /// redirect loop was detected or redirect limit was exhausted.
    pub fn execute(&self, request: Request) -> Pending {
        match request.try_into() {
            Ok((url, req)) => {
                // Prepare the future request by ensuring we use the exact same Service instance
                // for both poll_ready and call.
                match *self.inner {
                    ClientRef::Boxed(ref service) => Pending::BoxedRequest {
                        url: Some(url),
                        fut: service.clone().oneshot(req),
                    },
                    ClientRef::Generic(ref service) => Pending::GenericRequest {
                        url: Some(url),
                        fut: Box::pin(service.clone().oneshot(req)),
                    },
                }
            }
            Err(err) => Pending::Error { error: Some(err) },
        }
    }
}

impl tower_service::Service<Request> for Client {
    type Response = Response;
    type Error = Error;
    type Future = Pending;

    #[inline(always)]
    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline(always)]
    fn call(&mut self, req: Request) -> Self::Future {
        self.execute(req)
    }
}

impl tower_service::Service<Request> for &'_ Client {
    type Response = Response;
    type Error = Error;
    type Future = Pending;

    #[inline(always)]
    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline(always)]
    fn call(&mut self, req: Request) -> Self::Future {
        self.execute(req)
    }
}

use std::sync::{Arc,Mutex};
use std::net::{SocketAddr,ToSocketAddrs};
use std::io;
use std::{thread, str};

use tokio::io::{AsyncReadExt, AsyncWriteExt, stream_reader};
use tokio::net::{TcpListener,TcpStream};
use tokio::net::tcp::{OwnedReadHalf,OwnedWriteHalf};
use tokio::sync::mpsc;

use prometheus::{CounterVec,HistogramVec,Encoder,TextEncoder};
use clap::{Arg, App, crate_version};
use tracing::{info, warn, error, debug, info_span, Instrument, Level};
use tracing_subscriber::{FmtSubscriber, EnvFilter};
use lazy_static::lazy_static;

#[macro_use] extern crate prometheus;
#[macro_use] extern crate rouille;

use mongoproxy::jaeger_tracing;
use mongoproxy::dstaddr;
use mongoproxy::appconfig::{AppConfig};
use mongoproxy::tracker::{MongoStatsTracker};
use mongoproxy::mongodb::{MsgHeader, MongoMessage};


type BufBytes = Result<bytes::Bytes, io::Error>;

const JAEGER_ADDR: &str = "127.0.0.1:6831";
const ADMIN_PORT: &str = "9898";
const SERVICE_NAME: &str = "mongoproxy";

lazy_static! {
    static ref MONGOPROXY_RUNTIME_INFO: CounterVec =
        register_counter_vec!(
            "mongoproxy_runtime_info",
            "Runtime information about Mongoproxy",
            &["version", "proxy", "service_name", "log_mongo_messages", "enable_jaeger"]).unwrap();

    static ref CONNECTION_COUNT_TOTAL: CounterVec =
        register_counter_vec!(
            "mongoproxy_client_connections_established_total",
            "Total number of client connections established",
            &["client"]).unwrap();

    static ref DISCONNECTION_COUNT_TOTAL: CounterVec =
        register_counter_vec!(
            "mongoproxy_client_disconnections_total",
            "Total number of client disconnections",
            &["client"]).unwrap();

    static ref CONNECTION_ERRORS_TOTAL: CounterVec =
        register_counter_vec!(
            "mongoproxy_client_connection_errors_total",
            "Total number of errors from handle_connections",
            &["client"]).unwrap();

    static ref SERVER_CONNECT_TIME_SECONDS: HistogramVec =
        register_histogram_vec!(
            "mongoproxy_server_connect_time_seconds",
            "Time it takes to look up and connect to a server",
            &["server_addr"]).unwrap();
}

#[tokio::main]
async fn main() {
    let matches = App::new("mongoproxy")
        .version(crate_version!())
        .about("Proxies MongoDb requests to obtain metrics")
        .arg(Arg::with_name("proxy")
            .long("proxy")
            .value_name("local-port[:remote-host:remote-port]")
            .help("Port the proxy listens on (sidecar) and optionally\na target hostport (for static proxy)")
            .takes_value(true)
            .required(true))
        .arg(Arg::with_name("log_mongo_messages")
            .long("log-mongo-messages")
            .help("Log the contents of MongoDb messages (adds full BSON parsing)")
            .takes_value(false)
            .required(false))
        .arg(Arg::with_name("enable_jaeger")
            .long("enable-jaeger")
            .help("Enable distributed tracing with Jaeger")
            .takes_value(false)
            .required(false))
        .arg(Arg::with_name("jaeger_addr")
            .long("jaeger-addr")
            .value_name("Jaeger agent host:port")
            .help("Jaeger agent hostport to send traces to (compact thrift protocol)")
            .takes_value(true)
            .required(false))
        .arg(Arg::with_name("service_name")
            .long("service-name")
            .value_name("SERVICE_NAME")
            .help("Service name that will be used in Jaeger traces and metric labels")
            .takes_value(true))
        .arg(Arg::with_name("admin_port")
            .long("admin-port")
            .value_name("ADMIN_PORT")
            .help(&format!("Port the admin endpoints listens on (metrics and health). Default {}", ADMIN_PORT))
            .takes_value(true))
        .get_matches();

    let admin_port = matches.value_of("admin_port").unwrap_or(ADMIN_PORT);
    let admin_addr = format!("0.0.0.0:{}", admin_port);
    let service_name = matches.value_of("service_name").unwrap_or(SERVICE_NAME);
    let log_mongo_messages = matches.occurrences_of("log_mongo_messages") > 0;
    let enable_jaeger = matches.occurrences_of("enable_jaeger") > 0;
    let jaeger_addr = lookup_address(matches.value_of("jaeger_addr").unwrap_or(JAEGER_ADDR)).unwrap();

    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::TRACE)
        .with_env_filter(EnvFilter::from_default_env())
        .finish();

    tracing::subscriber::set_global_default(subscriber)
        .expect("setting default trace subscriber failed");

    info!("MongoProxy v{}", crate_version!());

    start_admin_listener(&admin_addr);
    info!("Admin endpoint at http://{}", admin_addr);

    let proxy_spec = matches.value_of("proxy").unwrap();
    let (local_hostport, remote_hostport) = parse_proxy_addresses(proxy_spec).unwrap();

    let app = AppConfig::new(
        jaeger_tracing::init_tracer(enable_jaeger, &service_name, jaeger_addr),
        log_mongo_messages,
    );

    MONGOPROXY_RUNTIME_INFO.with_label_values(&[
        crate_version!(),
        &proxy_spec,
        &service_name,
        if log_mongo_messages { "true" } else { "false" },
        if enable_jaeger { "true" } else { "false" } ],
    ).inc();

    run_accept_loop(local_hostport, remote_hostport, &app).await;
}

// Accept connections in a loop and spawn a task to proxy them. If remote address is not explicitly
// specified attempt to proxy to the original destination obtained with SO_ORIGINAL_DST socket
// option.
//
// Never returns.
async fn run_accept_loop(local_addr: String, remote_addr: String, app: &AppConfig)
{
    if remote_addr.is_empty() {
        info!("Proxying {} -> <original dst>", local_addr);
    } else {
        info!("Proxying {} -> {}", local_addr, remote_addr);
    }

    let mut listener = TcpListener::bind(&local_addr).await.unwrap();

    loop {
        match listener.accept().await {
            Ok((stream, peer_addr)) => {
                let client_ip_port = peer_addr.to_string();
                let client_addr = format_client_address(&peer_addr);

                let server_addr = if remote_addr.is_empty() {
                    if let Some(sockaddr) = dstaddr::orig_dst_addr(&stream) {
                        // This only assumes that NATd connections are received
                        // and thus always have a valid target address. We expect
                        // iptables rules to be in place to block direct access
                        // to the proxy port.
                        debug!("Original destination address: {:?}", sockaddr);
                        sockaddr.to_string()
                    } else {
                        error!("Host not set and destination address not found: {}", client_addr);
                        // TODO: Increase a counter
                        continue;
                    }
                } else {
                    remote_addr.clone()
                };

                let app = app.clone();
                let server_ip_port = server_addr.clone();

                CONNECTION_COUNT_TOTAL.with_label_values(&[&client_addr.to_string()]).inc();

                let conn_handler = async move {
                    info!("new connection from {}", client_addr);
                    match handle_connection(&server_addr, stream, app).await {
                        Ok(_) => {
                            info!("{} closing connection.", client_addr);
                            DISCONNECTION_COUNT_TOTAL
                                .with_label_values(&[&client_addr.to_string()])
                                .inc();
                        },
                        Err(e) => {
                            warn!("{} connection error: {}", client_addr, e);
                            CONNECTION_ERRORS_TOTAL
                                .with_label_values(&[&client_addr.to_string()])
                                .inc();
                        },
                    };
                };

                tokio::spawn(
                    conn_handler.instrument(
                        tracing::info_span!("handle_connection",
                            client_addr = client_ip_port.as_str(),
                            server_addr = server_ip_port.as_str()))
                );
            },
            Err(e) => {
                warn!("accept: {:?}", e)
            },
        }
    }
}

// Open a connection to the server and start passing bytes between the client and the server. Also
// split the traffic to MongoDb protocol parser, so that we can get some stats out of this.
//
// The philosophy here is that we will not change any of the bytes that are passed between the
// client and the server. Instead we fork off a stream and send it to a separate tracker task,
// which then parses the messages and collects metrics from it. Should the tracker fail, the
// proxy still remains operational.
//

async fn handle_connection(server_addr: &str, client_stream: TcpStream, app: AppConfig)
    -> Result<(), io::Error>
{
    info!("connecting to server: {}", server_addr);
    let timer = SERVER_CONNECT_TIME_SECONDS.with_label_values(&[server_addr]).start_timer();
    let server_addr = lookup_address(server_addr)?;
    let server_stream = TcpStream::connect(&server_addr).await?;
    timer.observe_duration();

    let client_addr = format_client_address(&client_stream.peer_addr()?);

    let log_mongo_messages = app.log_mongo_messages;
    let tracing_enabled = app.tracer.is_some();

    let tracker = Arc::new(Mutex::new(
            MongoStatsTracker::new(
                &client_addr,
                &server_addr.to_string(),
                server_addr,
                app)));
    let client_tracker = tracker.clone();
    let server_tracker = tracker.clone();

    client_stream.set_nodelay(true)?;
    server_stream.set_nodelay(true)?;

    // Start the trackers to parse and track MongoDb messages from the input stream. This works by
    // having the proxy tasks send a copy of the bytes over a channel and process that channel
    // as a stream of bytes, extracting MongoDb messages and tracking the metrics from there.

    let (client_tx, client_rx): (mpsc::Sender<BufBytes>, mpsc::Receiver<BufBytes>) = mpsc::channel(32);
    let (server_tx, server_rx): (mpsc::Sender<BufBytes>, mpsc::Receiver<BufBytes>) = mpsc::channel(32);

    let signal_client = client_tx.clone();
    let signal_server = server_tx.clone();

    tokio::spawn(async move {
        track_messages(client_rx, log_mongo_messages, tracing_enabled, move |hdr, msg| {
            let mut tracker = client_tracker.lock().unwrap();
            tracker.track_client_request(&hdr, &msg);
        }).await?;
        Ok::<(), io::Error>(())
    }.instrument(info_span!("client tracker")));

    tokio::spawn(async move {
        track_messages(server_rx, log_mongo_messages, false, move |hdr, msg| {
            let mut tracker = server_tracker.lock().unwrap();
            tracker.track_server_response(hdr, msg);
        }).await?;
        Ok::<(), io::Error>(())
    }.instrument(info_span!("server tracker")));

    // Now start proxying bytes between the client and the server.

    let (mut read_client, mut write_client) = client_stream.into_split();
    let (mut read_server, mut write_server) = server_stream.into_split();

    let client_task = async {
        proxy_bytes(&mut read_client, &mut write_server, client_tx, signal_server).await?;
        Ok::<(), io::Error>(())
    }.instrument(info_span!("client proxy"));

    let server_task = async {
        proxy_bytes(&mut read_server, &mut write_client, server_tx, signal_client).await?;
        Ok::<(), io::Error>(())
    }.instrument(info_span!("server proxy"));

    match tokio::try_join!(client_task, server_task) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(()),
        Err(e) => Err(e),
    }
}

// Move bytes between sockets, forking the byte stream into a mpsc channel
// for processing. Another channel is used to notify the other tracker of
// failures.
async fn proxy_bytes(
    read_from: &mut OwnedReadHalf,
    write_to: &mut OwnedWriteHalf,
    mut tracker_channel: mpsc::Sender<BufBytes>,
    mut notify_channel: mpsc::Sender<BufBytes>,
) -> Result<(), io::Error>
{
    let mut tracker_ok = true;

    loop {
        let mut buf = [0; 1024];
        let len = read_from.read(&mut buf).await?;

        if len > 0 {
            write_to.write_all(&buf[0..len]).await?;

            if tracker_ok {
                let bytes = bytes::Bytes::copy_from_slice(&buf[..len]);

                if let Err(e) = tracker_channel.send(Ok(bytes)).await {
                    error!("error sending to tracker, stop: {}", e);
                    tracker_ok = false;

                    // Let the other side know that we're closed.
                    let notification = io::Error::new(
                        io::ErrorKind::UnexpectedEof, "notify channel close");
                    let _ = notify_channel.send(Err(notification)).await;
                }
            }
        } else {
            // EOF on read, return Err to signal try_join! to return
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF"));
        }
    }
}

// Process the mpsc channel as a byte stream, parsing MongoDb messages
// and sending them off to a tracker.
async fn track_messages<F>(
    rx: mpsc::Receiver<BufBytes>,
    log_mongo_messages: bool,
    collect_tracing_data: bool,
    mut tracker_fn: F
) -> Result<(), io::Error>
    where F: FnMut(MsgHeader, MongoMessage)
{
    let mut s = stream_reader(rx);
    loop {
        match MongoMessage::from_reader(&mut s, log_mongo_messages, collect_tracing_data).await {
            Ok((hdr, msg)) => {
                tracker_fn(hdr, msg);
            },
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                return Ok(());
            },
            Err(e) => {
                error!("Tracker failed: {}", e);
                return Err(e);
            }
        }
    }
}

fn lookup_address(addr: &str) -> std::io::Result<SocketAddr> {
    if let Some(sockaddr) = addr.to_socket_addrs()?.next() {
        debug!("{} resolves to {}", addr, sockaddr);
        return Ok(sockaddr);
    }
    Err(io::Error::new(io::ErrorKind::AddrNotAvailable, "no usable address found"))
}

// Return the peer address of the stream without the :port
fn format_client_address(sockaddr: &SocketAddr) -> String {
    let mut addr_str = sockaddr.to_string();
    if let Some(pos) = addr_str.find(':') {
        let _ = addr_str.split_off(pos);
    }
    addr_str
}

// Parse the local and remote address pair from provided proxy definition
fn parse_proxy_addresses(proxy_def: &str) -> Result<(String,String), io::Error> {
    if let Some(pos) = proxy_def.find(':') {
        let (local_port, remote_hostport) = proxy_def.split_at(pos);
        let local_addr = format!("0.0.0.0:{}", local_port);

        Ok((local_addr, remote_hostport[1..].to_string()))
    } else {
        Ok((format!("0.0.0.0:{}", proxy_def), String::from("")))
    }
}

pub fn start_admin_listener(endpoint: &str) {
    let endpoint = endpoint.to_owned();
    thread::spawn(||
        rouille::start_server(endpoint, move |request| {
            router!(request,
                (GET) (/) => {
                    rouille::Response::html(
                        "<a href='/metrics'>metrics</a>\n<br>\n\
                         <a href='/health'>health</a>\n")
                },
                (GET) (/health) => {
                    rouille::Response::text("OK")
                },
                (GET) (/metrics) => {
                    let encoder = TextEncoder::new();
                    let metric_families = prometheus::gather();
                    let mut buffer = vec![];
                    encoder.encode(&metric_families, &mut buffer).unwrap();
                    rouille::Response::from_data("text/plain", buffer)
                },
                _ => rouille::Response::empty_404()
            )
        })
    );
}

#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

extern crate alloc;

use core::f32::consts::E;
use core::str::FromStr;

use alloc::borrow::Cow;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;
use alloc::{format, vec};

use defmt::{dbg, info, unwrap, warn, Debug2Format, Format};
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{dns, Ipv4Address, Stack};
use embassy_stm32::eth::{generic_smi::GenericSMI, Ethernet};
use embassy_stm32::peripherals::{self, ETH, RNG};
use embassy_stm32::time::mhz;
use embassy_stm32::Config;
use embassy_time::{Duration, Instant, Timer};
use embedded_io_async::{Read, Write};

use futures::TryFutureExt;
use heapless::String;
use rustls::client::{ClientConnectionData, InvalidDnsNameError, LlClientConnection};
use rustls::server::danger::DnsName;
use rustls::version::{TLS12, TLS13};
use rustls::{AppDataRecord, ClientConfig, InsufficientSizeError, LlState, RootCertStore};
use rustls::{EncodeError, LlStatus};
use rustls_pemfile::Item;

use stm32_rustls::democryptoprovider::DemoCryptoProvider;
use stm32_rustls::demotimeprovider::SINCE_START;
use stm32_rustls::{self as _, board::Board};
use stm32_rustls::{demotimeprovider, init_call_to_ntp_server, init_heap, network_task_init};
use {defmt_rtt as _, panic_probe as _};

// url scheme = https://
//const SERVER_NAME: &str = "rust-lang.org";
const SERVER_NAME: &str = "rust-lang.org";
const PORT: u16 = 1443;
pub static CRYPTO_PROVIDER: &'static dyn rustls::crypto::CryptoProvider = &DemoCryptoProvider;
type Device = Ethernet<'static, ETH, GenericSMI>;

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<Device>) -> ! {
    stack.run().await
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let mut config = Config::default();
    config.rcc.sys_ck = Some(mhz(100));
    let p = embassy_stm32::init(config);
    let board = Board::new(p);
    warn!("before stack");
    let stack = network_task_init(spawner, board).await;
    warn!("after stack");

    // Done sequentially now
    // Launch network task
    unwrap!(spawner.spawn(net_task(stack)));
    // why does this work, is it doing a background task out of its
    //stack.run().await;
    stack.wait_config_up().await;

    init_heap();

    init_call_to_ntp_server(stack).await;

    let mut root_store = RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    // Not needed for rust-lang.org
    // necessary for local
    let mut certfile: &[_] = include_bytes!("/home/aissata/.local/share/mkcert/rootCA.pem");
    let mut certs = vec![];
    while let Ok(Some((item, rest))) = rustls_pemfile::read_one_from_slice(certfile) {
        certfile = rest;
        if let Item::X509Certificate(cert) = item {
            certs.push(cert);
        }
    }
    dbg!(certs.len());
    root_store.add_parsable_certificates(certs);

    let mut client_config = ClientConfig::builder_with_provider(CRYPTO_PROVIDER)
        .with_safe_defaults()
        .dangerous()
        .with_custom_certificate_verifier(stm32_rustls::certificate_verifier(root_store))
        .with_no_client_auth();

    let now: Instant = Instant::now();
    SINCE_START.lock().await.replace(now);

    client_config.time_provider = demotimeprovider::time_provider();

    //TLS starts here
    let mut conn = LlClientConnection::new(
        Arc::new(client_config),
        rustls::ServerName::DnsName(DnsName::try_from(SERVER_NAME.to_string()).unwrap()),
    )
    .unwrap();

    spawner.spawn(process_state(stack, conn));
}

#[embassy_executor::task]
async fn process_state(stack: &'static Stack<Device>, mut conn: LlClientConnection) {
    let mut incoming_tls: [u8; 16384] = [0; 16 * 1024];
    let mut incoming_used = 0;

    let mut outgoing_tls: Vec<u8> = vec![];
    let mut outgoing_used = 0;
    let mut rx_buffer = [0; 1024];
    let mut tx_buffer = [0; 1024];
    loop {
        let mut open_connection = true;
        let request = http_request("localhost");

        let mut socket = embassy_net::tcp::TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(10)));

        socket.set_keep_alive(Some(Duration::from_millis(100)));
        //let remote_endpoint = (Ipv4Address::new(52, 85, 242, 46), PORT);
        let remote_endpoint = (Ipv4Address::new(192, 168, 50, 67), PORT);
        //let remote_endpoint = (Ipv4Address::new(127, 0, 0, 1), PORT);
        let connection_result = socket.connect(remote_endpoint).await;

        match connection_result {
            Ok(_) => info!("connection worked",),
            Err(e) => info!("connection error {}", &e),
        }

        while true {
            let LlStatus { discard, state } = conn
                .process_tls_records(&mut incoming_tls[..incoming_used])
                .unwrap();

            socket.set_keep_alive(Some(Duration::from_millis(100)));
            match state {
                LlState::MustEncodeTlsData(mut state) => {
                    dbg!("State MustEncode");
                    let written = match state.encode(&mut outgoing_tls[outgoing_used..]) {
                        Ok(written) => {
                            info!("Written bytes {}", written);
                            Ok(written)
                        }
                        Err(e) => match e {
                            EncodeError::InsufficientSize(InsufficientSizeError {
                                required_size,
                            }) => {
                                let new_len = outgoing_used + required_size;
                                outgoing_tls.resize(new_len, 0);
                                match state.encode(&mut outgoing_tls[outgoing_used..]) {
                                    Ok(w) => {
                                        info!("Written bytes {}", w);
                                        Ok(w)
                                    }
                                    Err(e) => Err(e),
                                }
                            }
                            EncodeError::AlreadyEncoded => Err(e),
                        },
                    };
                    outgoing_used += written.unwrap();
                    info!("outgoing_used {}", outgoing_used);
                }
                LlState::MustTransmitTlsData(state) => {
                    info!("Entering MustTransmitTlsData");

                    let _ = socket
                        .write_all(&outgoing_tls[..outgoing_used])
                        .await
                        .unwrap();
                    info!("Going to flush after write all");
                    socket.flush().await.unwrap();

                    outgoing_used = 0;
                    info!("State MustTransmit. state.done()");
                    state.done();
                }
                LlState::NeedsMoreTlsData { num_bytes } => {
                    info!("State NeedsMoreTlsData.");
                    let read = socket.read(&mut incoming_tls[incoming_used..]).await;
                    info!("After read");
                    info!("State NeedsMoreTls. Result of socket.read() {}", read);
                    incoming_used += read.unwrap();
                    info!("State NeedsMoreTls. incoming_used {}", incoming_used);
                }
                _ => info!("not managed state should panic"),
            }
            // discard TLS records
            // discard will kick in after sending
            if discard != 0 {
                assert!(discard <= incoming_used);
                dbg!(discard);
                incoming_tls.copy_within(discard..incoming_used, 0);
                incoming_used -= discard;
            }
        }
    }
}

fn http_request(server_name: &str) -> String<1024> {
    const HTTP_SEPARATOR: &str = "\r\n";

    let lines = [
        Cow::Borrowed("GET / HTTP/1.1"),
        format!("Host: {server_name}").into(),
        "Connection: close".into(),
        "Accept-Encoding: identity".into(),
        "".into(), // body
    ];

    let mut req = String::new();
    for line in lines {
        let _ = req.push_str(&line);
        let _ = req.push_str(HTTP_SEPARATOR);
    }

    req
}

#[derive(Debug)]
enum Error {
    RustLsEncodeError(EncodeError),
}

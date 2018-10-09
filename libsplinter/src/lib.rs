// Copyright 2018 Cargill Incorporated
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

extern crate rustls;
extern crate webpki;
#[macro_use]
extern crate protobuf;
extern crate bytes;
#[macro_use]
extern crate log;
extern crate byteorder;
extern crate messaging;

use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use bytes::Bytes;
use rustls::{
    AllowAnyAuthenticatedClient, Certificate, ClientConfig, ClientSession, NoClientAuth,
    PrivateKey, ServerConfig, ServerSession, Session, Stream, SupportedCipherSuite,
};
use std::collections::HashMap;
use std::fs;
use std::io::{stdout, BufReader, ErrorKind, Read, Write};
use std::mem;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::{mpsc, Arc, Mutex};
use std::{thread, time};

use messaging::protocol::{Message, MessageType};

/// Shorthand for the transmit half of the message channel.
pub type Tx = mpsc::Sender<Bytes>;

/// Shorthand for the receive half of the message channel.
pub type Rx = mpsc::Receiver<Bytes>;

pub struct Shared {
    pub peers: HashMap<SocketAddr, Tx>,
    pub services: HashMap<SocketAddr, Tx>,
    pub channels: HashMap<String, Channel>,
}

impl Shared {
    /// Create a new, empty, instance of `Shared`.
    pub fn new() -> Shared {
        Shared {
            peers: HashMap::new(),
            services: HashMap::new(),
            channels: HashMap::new(),
        }
    }
}

pub struct Channel {
    channel_id: String,
    peers: HashMap<String, Connection>,
}

impl Channel {
    fn new(channel_id: String, peers: HashMap<String, Connection>) -> Channel {
        Channel { channel_id, peers }
    }
}

// ClientConfig and ServerConfig should be made once and reused for each connection that is
// created.
#[derive(Debug)]
pub enum SessionType {
    client(rustls::ClientSession),
    server(rustls::ServerSession),
}

pub enum ConfigType {
    client(rustls::ClientConfig),
    server(rustls::ServerConfig),
}

pub enum ConnectionType {
    Network,
    Service,
}

pub enum ConnectionState {
    Running,
    Closing,
    Closed,
}

/// This is a connection which has been accepted by the server,
/// and is currently being served.
///
/// It has a TCP-level stream, and some
/// other state/metadata.
pub struct Connection {
    connection_state: ConnectionState,
    state: Arc<Mutex<Shared>>,
    addr: SocketAddr,
    socket: TcpStream,
    session: SessionType,
    connection_type: ConnectionType,
    rx: Rx,
}

impl Connection {
    pub fn new(
        socket: TcpStream,
        session: ConfigType,
        state: Arc<Mutex<Shared>>,
        dns_name: Option<String>,
        connection_type: ConnectionType,
    ) -> Connection {
        let session = match session {
            ConfigType::client(client_config) => {
                if let Some(name) = dns_name {
                    SessionType::client(create_client_session(client_config, name))
                } else {
                    panic!("No dns_name provided for client session")
                }
            }
            ConfigType::server(server_config) => {
                SessionType::server(create_server_session(server_config))
            }
        };

        // Create a channel for this peer
        let (tx, rx) = mpsc::channel();
        let addr = socket.peer_addr().unwrap();
        // Add an entry for this `Peer` in the shared state map.
        match connection_type {
            ConnectionType::Network => {
                state.lock().unwrap().peers.insert(addr, tx);
            }
            ConnectionType::Service => {
                state.lock().unwrap().services.insert(addr, tx);
            }
        }

        Connection {
            connection_state: ConnectionState::Running,
            state,
            addr,
            socket,
            session,
            connection_type,
            rx,
        }
    }

    fn handshake(&mut self) -> bool {
        match &mut self.session {
            SessionType::server(session) => {
                if session.is_handshaking() {
                    match session.complete_io(&mut self.socket) {
                        Ok(_) => return true,
                        Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                            return false;
                        }
                        Err(err) => panic!("Error {}", err),
                    };
                } else {
                    return true;
                }
            }
            SessionType::client(session) => {
                if session.is_handshaking() {
                    match session.complete_io(&mut self.socket) {
                        Ok(_) => return true,
                        Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                            return false;
                        }
                        Err(err) => panic!("Error {}", err),
                    };
                    return false;
                } else {
                    return true;
                }
            }
        };
    }

    fn read(&mut self) -> bool {
        let mut msg = Message::new();
        match &mut self.session {
            SessionType::server(session) => {
                if session.wants_read() {
                    match session.read_tls(&mut self.socket) {
                        Ok(n) => n,
                        Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                            return false;
                        }
                        Err(err) => panic!("Error {}", err),
                    };
                    match session.process_new_packets() {
                        Ok(n) => n,
                        Err(err) => panic!("Error {}", err),
                    };

                    let mut msg_len_buff = vec![0; mem::size_of::<u32>()];
                    session.read_exact(&mut msg_len_buff).unwrap();
                    let msg_size =
                        msg_len_buff.as_slice().read_u32::<BigEndian>().unwrap() as usize;

                    // Read Message
                    let mut msg_buff = vec![0; msg_size];
                    session.read_exact(&mut msg_buff).unwrap();

                    msg = protobuf::parse_from_bytes::<Message>(&msg_buff).unwrap();

                    println!("{:?}", msg,);
                };
            }
            SessionType::client(session) => {
                if session.wants_read() {
                    match session.read_tls(&mut self.socket) {
                        Ok(n) => n,
                        Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                            return false;
                        }
                        Err(err) => panic!("Error {}", err),
                    };
                    match session.process_new_packets() {
                        Ok(n) => n,
                        Err(err) => panic!("Error {}", err),
                    };

                    let mut msg_len_buff = vec![0; mem::size_of::<u32>()];
                    session.read_exact(&mut msg_len_buff).unwrap();
                    let msg_size =
                        msg_len_buff.as_slice().read_u32::<BigEndian>().unwrap() as usize;

                    // Read Message
                    let mut msg_buff = vec![0; msg_size];
                    session.read_exact(&mut msg_buff).unwrap();

                    msg = protobuf::parse_from_bytes::<Message>(&msg_buff).unwrap();

                    println!("{:?}", msg,);
                };
            }
        };

        match msg.get_message_type() {
            MessageType::UNSET => return false,
            MessageType::HEARTBEAT_REQUEST => {
                let mut response = Message::new();
                response.set_message_type(MessageType::HEARTBEAT_RESPONSE);
                self.respond(response);
            }
            MessageType::HEARTBEAT_RESPONSE => (),
            _ => self.gossip_message(msg)

        };
        return true;
    }

    fn write(&mut self, buf: &[u8]) {
        match &mut self.session {
            SessionType::server(session) => {
                match session.write_tls(&mut self.socket) {
                    Ok(n) => n,
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                        return;
                    }
                    Err(err) => panic!("Error {}", err),
                };
                let n = match session.write(buf) {
                    Ok(n) => n,
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                        return;
                    }
                    Err(err) => panic!("Error {}", err),
                };
                println!("Wrote {}", n)
            }
            SessionType::client(session) => {
                match session.write_tls(&mut self.socket) {
                    Ok(n) => n,
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                        return;
                    }
                    Err(err) => panic!("Error {}", err),
                };
                let n = match session.write(buf) {
                    Ok(n) => n,
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                        return;
                    }
                    Err(err) => panic!("Error {}", err),
                };
                println!("Wrote {}", n)
            }
        };
    }

    pub fn handle_msg(&mut self) {
        loop {
            let done = self.handshake();
            if done {
                break;
            }
        }
        let mut count = 0;
        loop {
            if self.read() {
                count = 0;
            }

            if count == 10 {
                info!("Sending Heartbeat to {:?}", self.addr);
                let mut msg = Message::new();
                msg.set_message_type(MessageType::HEARTBEAT_REQUEST);
                let msg_bytes = pack_response(&msg);
                self.write(&msg_bytes);
                count = 0
            }
            count = count + 1;

            match self.rx.recv_timeout(time::Duration::from_millis(100)) {
                Ok(bytes) => {
                    // need to check if this is succesful and retry if not
                    self.write(&bytes);
                }
                Err(RecvTimeoutError) => continue,
                Err(err) => {
                    println!("Need to handle Error: {:?}", err);
                }
            }
        }
    }

    fn gossip_message(&mut self, msg: Message) {
        let msg_bytes = Bytes::from(pack_response(&msg));
        // If message received from service forward to nodes, if from nodes forward to services
        // This needs to eventually handle the message types
        match self.connection_type {
            ConnectionType::Network => {
                for (addr, tx) in &self.state.lock().unwrap().services {
                    //Don't send the message to ourselves
                    if *addr == self.addr {
                        println!("Service {} {:?}", addr, msg);
                        // The send only fails if the rx half has been
                        // dropped, however this is impossible as the
                        // `tx` half will be removed from the map
                        // before the `rx` is dropped.
                        tx.send(msg_bytes.clone()).unwrap();
                    }
                }
            }
            ConnectionType::Service => {
                for (addr, tx) in &self.state.lock().unwrap().peers {
                    //Don't send the message to ourselves
                    if *addr != self.addr {
                        println!("Peer {} {:?}", addr, msg);
                        // The send only fails if the rx half has been
                        // dropped, however this is impossible as the
                        // `tx` half will be removed from the map
                        // before the `rx` is dropped.
                        tx.send(msg_bytes.clone()).unwrap();
                    }
                }
            }
        }
    }

    fn respond(&mut self, msg: Message) {
        let msg_bytes = Bytes::from(pack_response(&msg));
        self.write(&msg_bytes);
    }

    fn add_connection() -> () {
        unimplemented!();
    }

    fn remove_connection() -> () {
        unimplemented!();
    }

    fn add_channel() -> () {
        unimplemented!();
    }

    fn remove_channel() -> () {
        unimplemented!();
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        match self.connection_type {
            ConnectionType::Network => {
                self.state.lock().unwrap().peers.remove(&self.addr);
            }
            ConnectionType::Service => {
                self.state.lock().unwrap().services.remove(&self.addr);
            }
        }
    }
}

// Loads the private key associated with a cert for creating the tls config
pub fn load_key(file_path: &str) -> PrivateKey {
    let keyfile = fs::File::open(file_path).expect("cannot open private key file");
    let mut reader = BufReader::new(keyfile);
    let keys = rustls::internal::pemfile::pkcs8_private_keys(&mut reader).unwrap();
    assert!(keys.len() == 1);
    keys[0].clone()
}

// Loads the certifcate that should be connected to a tls config
pub fn load_cert(file_path: &str) -> Vec<Certificate> {
    let certfile = fs::File::open(file_path).expect("cannot open certificate file");
    let mut reader = BufReader::new(certfile);
    rustls::internal::pemfile::certs(&mut reader).unwrap()
}

// Creates a Client config for tls communicating
pub fn create_client_config(
    ca_certs: Vec<Certificate>,
    client_certs: Vec<Certificate>,
    key: PrivateKey,
    cipher_suite: Vec<&'static SupportedCipherSuite>,
) -> ClientConfig {
    let mut config = rustls::ClientConfig::new();
    for cert in ca_certs {
        config.root_store.add(&cert);
    }
    config.set_single_client_cert(client_certs, key);
    config.ciphersuites = cipher_suite;
    config
}

// Creates a Client Session from the ClientConfig and dns_name associated with the server to
// connect to
pub fn create_client_session(config: ClientConfig, dns_name: String) -> ClientSession {
    let dns_name = webpki::DNSNameRef::try_from_ascii_str(&dns_name).unwrap();
    ClientSession::new(&Arc::new(config), dns_name)
}

// Creates a Server config for tls communicating
pub fn create_server_config(
    ca_certs: Vec<Certificate>,
    server_certs: Vec<Certificate>,
    key: PrivateKey,
) -> ServerConfig {
    let mut client_auth_roots = rustls::RootCertStore::empty();
    for cert in ca_certs {
        client_auth_roots.add(&cert).unwrap();
    }

    let auth = AllowAnyAuthenticatedClient::new(client_auth_roots);

    let mut config = ServerConfig::new(auth);
    config.key_log = Arc::new(rustls::KeyLogFile::new());
    config.set_single_cert(server_certs, key);
    config
}

// Creates a Server Session from the ServerConfig
pub fn create_server_session(config: ServerConfig) -> ServerSession {
    ServerSession::new(&Arc::new(config))
}

pub fn pack_response(msg: &Message) -> Vec<u8> {
    let raw_msg = protobuf::Message::write_to_bytes(msg).unwrap();
    let mut buff = Vec::new();

    buff.write_u32::<BigEndian>(raw_msg.len() as u32).unwrap();
    buff.write(&raw_msg).unwrap();

    buff
}
// LNP Node: node running lightning network protocol and generalized lightning
// channels.
// Written in 2020 by
//     Dr. Maxim Orlovsky <orlovsky@pandoracore.com>
//
// To the extent possible under law, the author(s) have dedicated all
// copyright and related and neighboring rights to this software to
// the public domain worldwide. This software is distributed without
// any warranty.
//
// You should have received a copy of the MIT License
// along with this software.
// If not, see <https://opensource.org/licenses/MIT>.

#![recursion_limit = "256"]
// Coding conventions
#![deny(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    unused_mut,
    unused_imports,
    dead_code,
    missing_docs
)]

//! Main executable for connectiond: lightning peer network connection
//! microservice.
//!
//! Program operations
//! ==================
//!
//! Bootstrapping
//! -------------
//!
//! Since this daemon must operate external P2P TCP socket, and TCP socket can
//! be either connected to the remote or accept remote connections; and we need
//! a daemon per connection, while the incoming TCP socket can't be transferred
//! between processes using IPC, the only option here is to have two special
//! cases of the daemon.
//!
//! The first one will open TCP socket in listening mode and wait for incoming
//! connections, forking on each one of them, passing the accepted TCP socket to
//! the child and continuing on listening to the new connections. (In
//! multi-thread mode, differentiated with `--threaded` argument, instead of
//! forking damon will launch a new thread).
//!
//! The second one will be launched by some control process and then commanded
//! (through command API) to connect to a specific remote TCP socket.
//!
//! These two cases are differentiated by a presence command-line option
//! `--listen` followed by a listening address to bind (IPv4, IPv6, Tor and TCP
//! port number) or `--connect` option followed by a remote address in the same
//! format.
//!
//! Runtime
//! -------
//!
//! The overall program logic thus is the following:
//!
//! In the process starting from `main()`:
//! - Parse cli arguments into a config. There is no config file, since the
//!   daemon can be started only from another control process (`lnpd`) or by
//!   forking itself.
//! - If `--listen` argument is present, start a listening version as described
//!   above and open TCP port in listening mode; wait for incoming connections
//! - If `--connect` argument is present, connect to the remote TCP peer
//!
//! In forked/spawned version:
//! - Acquire connected TCP socket from the parent
//!
//! From here, all actions must be taken by either forked version or by a daemon
//! launched from the control process:
//! - Split TCP socket and related transcoders into reading and writing parts
//! - Create bridge ZMQ PAIR socket
//! - Put both TCP socket reading ZMQ bridge write PAIR parts into a thread
//!   ("bridge")
//! - Open control interface socket
//! - Create run loop in the main thread for polling three ZMQ sockets:
//!   * control interface
//!   * LN P2P messages from intranet
//!   * bridge socket
//!
//! Node key
//! --------
//!
//! Node key, used for node identification and in generation of the encryption
//! keys, is read from the file specified in `--key-file` parameter, or (if the
//! parameter is absent) from `LNP_NODE_KEY_FILE` environment variable.

#![feature(never_type)]

#[macro_use]
extern crate log;
#[macro_use]
extern crate amplify_derive;

use amplify::internet::InetSocketAddr;
use clap::Clap;
use core::convert::TryFrom;
use log::LevelFilter;
use nix::unistd::{fork, ForkResult};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use lnp_node::connectiond::Opts;
use lnpbp::lnp::transport::FramingProtocol;
use lnpbp::lnp::zmqsocket::SocketLocator;
use lnpbp::lnp::{session, LocalNode, NodeAddr, PeerConnection, RemoteAddr};

/*
mod internal {
    #![allow(unused)]
    include!(concat!(env!("OUT_DIR"), "/configure_me_config.rs"));
}
 */

/// Choses type of service runtime (see `--listen` and `--connect` option
/// details in [`Opts`] structure.
#[derive(Clone, PartialEq, Eq, Debug, Display)]
pub enum P2pSocket {
    /// The service should listen for incoming connections on a certain
    /// TCP socket, which may be IPv4- or IPv6-based. For Tor hidden services
    /// use IPv4 TCP port proxied as a Tor hidden service in `torrc`.
    #[display("--listen={_0}")]
    Listen(RemoteAddr),

    /// The service should connect to the remote peer residing on the provided
    /// address, which may be either IPv4/v6 or Onion V2/v3 address (using
    /// onion hidden services will require
    /// DNS names, due to a censorship vulnerability issues and for avoiding
    /// leaking any information about th elocal node to DNS resolvers, are not
    /// supported.
    #[display("--connect={_0}")]
    Connect(NodeAddr),
}

impl From<Opts> for P2pSocket {
    fn from(opts: Opts) -> Self {
        if let Some(peer_addr) = opts.connect {
            Self::Connect(peer_addr)
        } else if let Some(bind_addr) = opts.listen {
            Self::Listen(match opts.overlay {
                FramingProtocol::FramedRaw => {
                    RemoteAddr::Ftcp(InetSocketAddr {
                        address: bind_addr
                            .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
                            .into(),
                        port: opts.port,
                    })
                }
                // TODO: (v2) implement overlay protocols
                _ => unimplemented!(),
            })
        } else {
            unreachable!(
                "Either `connect` or `listen` must be present due to Clap configuration"
            )
        }
    }
}

/// Final configuration resulting from data contained in config file environment
/// variables and command-line options. For security reasons node key is kept
/// separately.
#[derive(Clone, PartialEq, Eq, Debug, Display)]
#[display(Debug)]
pub struct SocketConfig {
    /// ZMQ RPC socket for transmitting lightning peer network messages
    pub msg_socket: SocketLocator,

    /// ZMQ RPC socket for internal daemon control bus
    pub ctl_socket: SocketLocator,

    /// If set, specifies SOCKS5 proxy used for Tor connectivity. Required if
    /// `p2p_socket` is set to `P2pSocket::Connect` with onion address.
    pub tor_socks5: SocketAddr,
}

fn main() {
    log::set_max_level(LevelFilter::Trace);
    info!("connectiond: lightning peer network connection microservice");

    let opts = Opts::parse();

    let local_node = LocalNode::new();

    let connection = match P2pSocket::from(opts) {
        P2pSocket::Listen(RemoteAddr::Ftcp(inet_addr)) => {
            use std::net::TcpListener;
            let listener = TcpListener::bind(
                SocketAddr::try_from(inet_addr)
                    .expect("Tor is not yet supported"),
            )
            .expect("Unable to bind to Lightning network peer socket");
            loop {
                let (stream, remote_addr) = listener
                    .accept()
                    .expect("Error accepting incpming peer connection");
                if let ForkResult::Child =
                    unsafe { fork().expect("Unable to fork child process") }
                {
                    continue;
                }
                let session =
                    session::Raw::with_ftcp_unencrypted(stream, inet_addr)
                        .expect(
                            "Unable to establish session with the remote peer",
                        );
                break PeerConnection::with(session);
            }
        }
        P2pSocket::Connect(node_addr) => {
            PeerConnection::connect(node_addr, &local_node)
                .expect("Unable to connect to the remote peer")
        }
        _ => unimplemented!(),
    };

    // let runtime = connectiond::Runtime::new(connection, socket_config);
    // runtime.run_loop();

    /*
    use self::internal::ResultExt;
    let (config_from_file, _) =
        internal::Config::custom_args_and_optional_files(std::iter::empty::<
            &str,
        >())
        .unwrap_or_exit();
     */
}
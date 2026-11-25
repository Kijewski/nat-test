// SPDX-FileCopyrightText: 2026 René Kijewski <crates.io@k6i.de>
// SPDX-License-Identifier: Apache-2.0

#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod stunlist;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Write, stdout};
use std::mem::DropGuard;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use clap::Parser;
use either::Either;
use indicatif::ProgressIterator;
use iprfc::{Filter, RFCs};
use rustix::io::Errno;
use rustix::termios::{OptionalActions, tcgetattr, tcsetattr};
use stunclient::StunClient;
use tokio::net::{UdpSocket, lookup_host};
use tokio::select;
use tokio::signal::ctrl_c;
use tokio::time::timeout;
use tracing::{info, warn};
use tracing_subscriber::util::SubscriberInitExt;

fn main() -> Result<(), Error> {
    let _termios_guard = tcgetattr(stdout()).ok().map(|termios| {
        DropGuard::new(termios, |termios| {
            let _: Result<(), Errno> = tcsetattr(stdout(), OptionalActions::Flush, &termios);
        })
    });

    let cli = Cli::parse();

    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(tracing::level_filters::LevelFilter::WARN.into())
        .from_env()?;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .compact()
        .finish()
        .try_init()?;

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(Error::TokioRt)?
        .block_on(cli.execute())
}

#[derive(Debug, Parser)]
#[command(version, about = ABOUT, long_about = LONG_ABOUT, propagate_version = true)]
struct Cli {
    /// IP address to bind to
    #[arg(default_value_t = Ipv4Addr::UNSPECIFIED.into())]
    addr: IpAddr,
    /// UDP port number to bind to
    #[arg(default_value = "0")]
    port: u16,
    /// Don't print messages (see the exit code instead)
    #[arg(short = 'q', long = "quiet")]
    quiet: bool,
}

const LONG_ABOUT: &str = {
    let mut content = include_str!("../README.md").trim_ascii().as_bytes();
    let content = loop {
        content = match content {
            [] => unreachable!(),
            [b'\n', b'\n', content @ ..] => break content,
            [_, content @ ..] => content,
        };
    };
    match str::from_utf8(content) {
        Ok(result) => result,
        Err(_) => unreachable!(),
    }
};

const ABOUT: &str = {
    let mut content = LONG_ABOUT.as_bytes();
    let mut idx = 0;
    while content[idx] != b'\n' {
        idx += 1;
    }
    while content.len() > idx {
        content = match content {
            [content @ .., _] => content,
            _ => unreachable!(),
        };
    }
    match str::from_utf8(content) {
        Ok(result) => result,
        Err(_) => unreachable!(),
    }
};

impl Cli {
    async fn execute(self) -> Result<(), Error> {
        let stunlist = stunlist::StunList::get().await?.content.into_iter();
        let mut stunlist = match self.quiet {
            true => Either::Left(stunlist),
            false => Either::Right(stunlist.progress()),
        };

        let sock_addr = SocketAddr::new(self.addr, self.port);
        let sock = UdpSocket::bind(sock_addr)
            .await
            .map_err(|err| Error::Bind(err, sock_addr))?;

        let mut map = BTreeMap::<_, BTreeSet<_>>::new();
        select! {
            biased;
            _ = ctrl_c() => (),
            _ = fill_map(&mut stunlist, &sock, &mut map) => (),
        };

        if !self.quiet {
            print_addresses(&map, &sock)?;
        }
        match map.len() {
            0 => Err(Error::NoAddresses),
            1 => Ok(()),
            _ => Err(Error::MultipleAddresses),
        }
    }
}

async fn fill_map(
    stunlist: &mut impl Iterator<Item = String>,
    sock: &UdpSocket,
    map: &mut BTreeMap<SocketAddr, BTreeSet<String>>,
) {
    for stun_server in stunlist {
        if let Some(address) = stun_lookup(stun_server.as_str(), sock).await {
            map.entry(address).or_default().insert(stun_server);
        }
    }
}

fn print_addresses(
    map: &BTreeMap<SocketAddr, BTreeSet<String>>,
    sock: &UdpSocket,
) -> Result<(), Error> {
    let mut stdout = stdout().lock();
    let mut iter = map.iter().fuse();
    match (iter.next(), iter.next()) {
        (None, _) => {}
        (Some((addr, servers)), None) => {
            let descr = if sock
                .local_addr()
                .is_ok_and(|local_addr| local_addr == *addr)
            {
                "direct connection"
            } else {
                "full cone, or restricted cone NAT"
            };
            writeln!(
                stdout,
                "\
                    You have a {descr}.\n\
                    All {} STUN servers agree on one external address + port: `{addr}`.\
                ",
                servers.len()
            )?;
        }
        (_, Some(_)) => {
            writeln!(
                stdout,
                "\
                    You have a symmetric NAT. Sucks to be you.\n\
                    {} STUN servers returned {} different external addresses/ports:\
                ",
                map.values().map(|set| set.len()).sum::<usize>(),
                map.len(),
            )?;
            for (addr, servers) in map {
                write!(stdout, "{addr} ({}) -> ", servers.len())?;
                for (idx, server) in servers.iter().enumerate() {
                    if idx > 0 {
                        write!(stdout, ", ")?;
                    }
                    write!(stdout, "`{}`", server.escape_debug())?;
                }
                writeln!(stdout)?;
            }
        }
    }
    Ok(())
}

async fn stun_lookup(stun_server: &str, sock: &UdpSocket) -> Option<SocketAddr> {
    #[derive(pretty_error_debug::Debug, thiserror::Error)]
    enum LookupError<'a> {
        #[error("Could not look up address `{}`.", .0.escape_debug())]
        Lookup(&'a str, #[source] std::io::Error),
        #[error("Timeout while looking up address `{}`.", .0.escape_debug())]
        Timeout(&'a str, #[source] tokio::time::error::Elapsed),
        #[error("Empty address list for address `{}`.", .0.escape_debug())]
        Empty(&'a str),
    }

    let stun_addrs = match timeout(LOOKUP_TIMEOUT, lookup_host(stun_server)).await {
        Ok(Ok(stun_addrs)) => {
            let mut stun_addrs = stun_addrs.peekable();
            match stun_addrs.peek() {
                Some(_) => Ok(stun_addrs),
                None => Err(LookupError::Empty(stun_server)),
            }
        }
        Ok(Err(err)) => Err(LookupError::Lookup(stun_server, err)),
        Err(err) => Err(LookupError::Timeout(stun_server, err)),
    };
    let stun_addrs = match stun_addrs {
        Ok(stun_addrs) => stun_addrs,
        Err(err) => {
            info!("{err:?}");
            return None;
        }
    };
    for stun_addr in stun_addrs {
        if is_special(stun_addr) {
            info!(
                "Ignoring STUN server address `{stun_addr}` for `{}`",
                stun_server.escape_debug(),
            );
            continue;
        }

        let query = StunClient::new(stun_addr).query_external_address_async(sock);
        if let Ok(Ok(sock_addr)) = timeout(QUERY_TIMEOUT, query).await {
            if is_special(sock_addr) {
                warn!(
                    "STUN server `{}` returned invalid address `{sock_addr}`",
                    stun_server.escape_debug(),
                );
                continue;
            }

            return Some(sock_addr);
        }
    }

    warn!(
        "Got no answer from STUN server `{}`.",
        stun_server.escape_debug()
    );
    None
}

fn is_special(addr: SocketAddr) -> bool {
    const FILTERS: iprfc::Subset = RFCs::filter(Filter::all());

    FILTERS.contains(&addr.ip())
}

#[derive(pretty_error_debug::Debug, thiserror::Error)]
enum Error {
    #[error("Could not set up tracing filter")]
    TracingEnv(#[from] tracing_subscriber::filter::FromEnvError),
    #[error("Could not set up tracing registry")]
    TracingInit(#[from] tracing_subscriber::util::TryInitError),
    #[error("Could not build Tokio runtime")]
    TokioRt(#[source] std::io::Error),
    #[error(transparent)]
    StunList(#[from] stunlist::StunListError),
    #[error("Could not bind to {1}")]
    Bind(#[source] std::io::Error, SocketAddr),
    #[error("No external address could be determined from any STUN server")]
    NoAddresses,
    #[error("Multiple socket addresses for multiple STUN connections returned")]
    MultipleAddresses,
    #[error("Cannot write to stdout")]
    StdOut(#[from] std::io::Error),
}

const LOOKUP_TIMEOUT: Duration = Duration::new(3, 0);
const QUERY_TIMEOUT: Duration = Duration::new(5, 0);

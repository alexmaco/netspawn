#![allow(unused_imports)]

use mio::net::TcpStream;
use mio_more::timer::{Timeout, Timer, TimerError};

use std::collections::*;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt::{Debug, Formatter};
use std::io;
use std::io::*;
use std::net::{self, IpAddr, SocketAddr};
use std::process::*;

use std::os::unix::io::*;

use std::collections::hash_map::Entry;
use std::collections::hash_map::Entry::*;

use mio::tcp::TcpListener;
use mio::unix::*;
use mio::{Events, Poll, PollOpt, Ready, Token};
use std::time::Duration;

fn start_server(path: &OsStr, args: &[OsString]) -> Result<Guardian> {
    let child = Command::new(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .args(args.iter())
        .spawn()?;
    Ok(Guardian { child })
}

// this exists because simply dropping a child just detaches the handle
// instead we want a RAII handle that kills it when we're done (i.e. the connection is closed)
struct Guardian {
    child: Child,
}

impl Drop for Guardian {
    fn drop(&mut self) {
        //eprintln!("killing child id {}", self.child.id());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Ord, PartialOrd, Hash)]
struct Link(usize);

struct Connection {
    guard: Guardian,
    tcp: TcpStream,
    child_raw_out: RawFd,
    net_in_tok: Token,
    child_out_tok: Token,
    //timer: Timer<Token>,
    timeout_token: Token,
    timeout: Timeout,
}

pub struct Server {
    tok_to_link: HashMap<Token, Link>,
    connections: HashMap<Link, Connection>,
    tok_gen: Token,
    link_gen: Link,
    child_path: OsString,
    child_args: Vec<OsString>,
    listener: TcpListener,
    poll: Poll,
    timer: Timer<Token>,
    acceptor_token: Token,
    timer_token: Token,
}

impl Server {
    pub fn new(port: u16, child_path: OsString, child_args: Vec<OsString>) -> Self {
        let listener = TcpListener::bind(&([0; 4], port).into()).expect("cannot listen");
        let poll = Poll::new().unwrap();
        let acceptor_token = Token(0);
        let timer_token = Token(1);

        Self {
            tok_to_link: HashMap::new(),
            connections: HashMap::new(),
            tok_gen: Token(10),
            link_gen: Link(10),
            child_path,
            child_args,
            listener,
            poll,
            timer: Timer::default(),
            acceptor_token,
            timer_token,
        }
    }

    fn next_tok(&mut self) -> Token {
        self.tok_gen.0 = self.tok_gen.0.wrapping_add(1);
        self.tok_gen
    }
    fn next_link(&mut self) -> Link {
        self.link_gen.0 = self.link_gen.0.wrapping_add(1);
        self.link_gen
    }

    pub fn run(&mut self) -> ! {
        self.poll
            .register(
                &self.listener,
                self.acceptor_token,
                Ready::writable() | Ready::readable(),
                PollOpt::edge(),
            )
            .unwrap();

        self.poll
            .register(
                &self.timer,
                self.timer_token,
                Ready::readable(),
                PollOpt::edge() | PollOpt::level(),
            )
            .unwrap();

        let mut events = Events::with_capacity(128);
        let mut scratch = vec![0; 1024 * 9];
        loop {
            // passing and reusing these to avoid redundant free&alloc on every wakeup
            let res = self.dispatch_events(&mut scratch, &mut events);
            if let Err(e) = res {
                eprintln!("Error dispatching events: {:?}", e);
            }
        }
    }

    fn dispatch_events(&mut self, mut scratch: &mut [u8], mut events: &mut Events) -> Result<()> {
        self.poll.poll(&mut events, None)?;

        for event in events.iter() {
            let tk = event.token();
            if tk == self.acceptor_token {
                let res = self.setup_connection();
                if let Err(e) = res {
                    eprintln!("Error creating connection: {:?}", e);
                }
                continue;
            } else if tk == self.timer_token {
                let mut tokens = Vec::new();
                while let Some(token) = self.timer.poll() {
                    tokens.push(token);
                }

                for token in tokens {
                    let link = match self.tok_to_link.get(&token) {
                        Some(l) => *l,
                        None => continue,
                    };
                    self.close_connection_and_child(link);
                }
            } else {
                let link = match self.tok_to_link.get(&tk) {
                    Some(l) => *l,
                    None => continue,
                };

                if self.process_link_token(link, tk, &mut scratch).is_err() {
                    self.close_connection_and_child(link);
                }
            }
        }
        Ok(())
    }

    fn process_link_token(&mut self, link: Link, tk: Token, mut scratch: &mut [u8]) -> Result<()> {
        let duration_timeout = Duration::from_millis(5000);

        let mut ent = match self.connections.entry(link) {
            Occupied(ent) => ent,
            _ => return Ok(()),
        };

        let conn = ent.get_mut();
        if tk == conn.net_in_tok {
            // net -> child
            let amt = conn.tcp.read(&mut scratch)?;
            if amt == 0 {
                return Err(ErrorKind::ConnectionReset.into());
            }

            let stdin = conn
                .guard
                .child
                .stdin
                .as_mut()
                .ok_or_else(|| io::Error::from(ErrorKind::BrokenPipe))?;
            stdin.write_all(&scratch[..amt])?;
        } else if tk == conn.child_out_tok {
            // child -> net
            let stdout = conn
                .guard
                .child
                .stdout
                .as_mut()
                .ok_or_else(|| io::Error::from(ErrorKind::BrokenPipe))?;
            let amt = stdout.read(&mut scratch)?;
            if amt == 0 {
                return Err(ErrorKind::ConnectionAborted.into());
            }

            conn.tcp.write_all(&scratch[..amt])?;
        } else {
            return Ok(()); // neither token matches, but we don't care
        }

        self.timer.cancel_timeout(&conn.timeout);
        conn.timeout = match self.timer.set_timeout(duration_timeout, conn.timeout_token) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("cannot set timeout: {:?}", e);
                return Err(io::Error::new(ErrorKind::Other, e.to_string()));
            }
        };
        Ok(())
    }

    fn close_connection_and_child(&mut self, link: Link) {
        if let Occupied(ent) = self.connections.entry(link) {
            let conn = ent.get();
            self.timer.cancel_timeout(&conn.timeout);
            let res = self.poll.deregister(&EventedFd(&conn.child_raw_out));
            if let Err(e) = res {
                eprintln!("Error deregistering child fd: {:?}", e);
            }
            let res = self.poll.deregister(&conn.tcp);
            if let Err(e) = res {
                eprintln!("Error deregistering poll: {:?}", e);
            }
            self.tok_to_link.remove(&conn.net_in_tok);
            self.tok_to_link.remove(&conn.child_out_tok);
            self.tok_to_link.remove(&conn.timeout_token);
            ent.remove_entry();
        } else {
            eprintln!("Error: link {:?} not found", link);
        }
    }

    fn setup_connection(&mut self) -> Result<()> {
        let duration_timeout = Duration::from_millis(5000);

        let (conn, _addr) = self.accept_conection(duration_timeout)?;

        let new_link = self.next_link();
        /*
        eprintln!(
            "new connection: pid {:?} link {:?} with {:?}",
            conn.guard.child.id(),
            new_link,
            addr
        );
        */
        self.tok_to_link.insert(conn.net_in_tok, new_link);
        self.tok_to_link.insert(conn.child_out_tok, new_link);
        self.tok_to_link.insert(conn.timeout_token, new_link);
        self.connections.insert(new_link, conn);
        Ok(())
    }

    fn accept_conection(&mut self, duration_timeout: Duration) -> Result<(Connection, SocketAddr)> {
        let (tcp, addr) = self.listener.accept()?;
        tcp.set_nodelay(true)?;
        let guard = start_server(&self.child_path, &self.child_args)?;
        let child_raw_out = match guard.child.stdout {
            Some(ref x) => x,
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "subprocess guard has no stdout handle",
                ))
            }
        }
        .as_raw_fd();
        let child_out_tok = self.next_tok();
        self.poll.register(
            &EventedFd(&child_raw_out),
            child_out_tok,
            Ready::readable(),
            PollOpt::edge() | PollOpt::level(),
        )?;

        let net_in_tok = self.next_tok();
        self.poll.register(
            &tcp,
            net_in_tok,
            Ready::readable(),
            PollOpt::edge() | PollOpt::level(),
        )?;

        let timeout_token = self.next_tok();
        let timeout = match self.timer.set_timeout(duration_timeout, timeout_token) {
            Ok(t) => t,
            _ => return Err(io::Error::new(io::ErrorKind::Other, "mio_more timer error")),
        };

        let conn = Connection {
            guard,
            tcp,
            child_raw_out,
            net_in_tok,
            child_out_tok,
            timeout_token,
            timeout,
        };

        Ok((conn, addr))
    }
}

fn main() {
    let options = Options::from_args();

    let child_path = OsString::from(options.cmd_and_args[0].clone());
    let child_args = options.cmd_and_args[1..]
        .iter()
        .map(OsString::from)
        .collect();

    let mut srv = Server::new(options.port, child_path, child_args);

    srv.run();
}

use structopt::*;

#[derive(StructOpt, Debug)]
#[structopt(about = "tcpserver/partial socat replacement, just without buffering issues")]
#[structopt(settings = &[clap::AppSettings::TrailingVarArg])]
struct Options {
    #[structopt(required = true, short, long, help = "the local port to listen on")]
    port: u16,

    #[structopt(required = true, help = "command to execute and its arguments")]
    cmd_and_args: Vec<String>,
}

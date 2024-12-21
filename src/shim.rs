use std::{
    io::{Error, Read, Write},
    net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, ToSocketAddrs},
    sync::{Arc, Condvar, Mutex},
};

use lazy_static::lazy_static;
use smoltcp::{
    iface::{Config, Context, Interface, SocketHandle, SocketSet},
    phy::{Loopback, Medium},
    socket::tcp::{ConnectError, ListenError, Socket},
    storage::RingBuffer,
    time::Instant,
    wire::{EthernetAddress, IpAddress, IpCidr},
};
// use tracing;

pub type SocketBuffer<'a> = RingBuffer<'a, u8>;
pub struct Engine {
    core: Arc<Mutex<Core>>,
    condvar: Arc<Condvar>,
}
struct Core {
    socketset: SocketSet<'static>,
    iface: Interface,
    device: Loopback, // for now
}

lazy_static! {
    static ref ENGINE: Arc<Engine> = Arc::new(Engine::new());
}

impl Engine {
    fn new() -> Self {
        Self {
            core: Arc::new(Mutex::new(Core::new())),
            condvar: Arc::new(Condvar::new()),
        }
    }
    fn add_socket(&self, socket: Socket<'static>) -> SocketHandle {
        self.core.lock().unwrap().add_socket(socket)
    }
    // fns to get sockets
    // Block until f returns Some(R), and then return R. Note that f may be called multiple times,
    // and it may be called spuriously.
    fn blocking<R>(&self, mut f: impl FnMut(&mut Core) -> Option<R>) -> R {
        let mut inner = self.core.lock().unwrap();
        println!("blocking(): polling from blocking");
        // Immediately poll, since we wait to have as up-to-date state as possible.
        inner.poll(&self.condvar);
        loop {
            // We'll need the polling thread to wake up and do work.
            // self.channel.send(()).unwrap();
            match f(&mut *inner) {
                Some(r) => {
                    // We have done work, so again, notify the polling thread.
                    // self.channel.send(()).unwrap();
                    return r;
                }
                None => {
                    println!("blocking(): blocking thread");
                    inner = self.condvar.wait(inner).unwrap();
                }
            }
        }
        // mutex dropped here.
    }
}

impl Core {
    fn new() -> Self {
        let config = Config::new(EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]).into()); // change later!
        let mut socketset = SocketSet::new(Vec::new());
        let mut device = Loopback::new(Medium::Ethernet);
        let mut iface = Interface::new(config, &mut device, Instant::now());
        iface.update_ip_addrs(|ip_addrs| {
            ip_addrs
                .push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8))
                .unwrap();
        });
        Self {
            socketset,
            device,
            iface,
        }
    }
    fn add_socket(&mut self, sock: Socket<'static>) -> SocketHandle {
        self.socketset.add(sock)
    }
    fn get_socket(&mut self, handle: SocketHandle) -> &Socket<'static> {
        self.socketset.get(handle)
    }
    fn get_mutable_socket(&mut self, handle: SocketHandle) -> &mut Socket<'static> {
        self.socketset.get_mut(handle)
    }
    fn poll(&mut self, waiter: &Condvar) -> bool {
        let res = self
            .iface
            .poll(Instant::now(), &mut self.device, &mut self.socketset);
        // When we poll, notify the CV so that other waiting threads can retry their blocking
        // operations.
        // println!("poll(): notify cv");
        // waiter.notify_all();
        res
    }
}

// a variant of std's tcplistener using smoltcp's api
pub struct SmolTcpListener {
    socket_handle: SocketHandle,
    local_addr: SocketAddr,
    port: u16,
}

impl SmolTcpListener {
    /* each_addr():
     * parameters:
     * helper function for bind()
     * processes each address given to see whether it can implement ToSocketAddr, then tries to
     * listen on that addr keeps trying each address until one of them successfully listens
     */
    fn each_addr<A: ToSocketAddrs>(
        sock_addrs: A,
        s: &mut Socket<'static>,
    ) -> Result<(u16, SocketAddr), ListenError> {
        let addrs = {
            match sock_addrs.to_socket_addrs() {
                Ok(addrs) => addrs,
                Err(e) => return Err(ListenError::InvalidState),
            }
        };
        for addr in addrs {
            match (*s).listen(addr.port()) {
                Ok(_) => return Ok((addr.port(), addr)),
                Err(_) => return Err(ListenError::Unaddressable),
            }
        }
        Err(ListenError::InvalidState) // is that the correct thing to return?
    }
    fn do_bind<A: ToSocketAddrs>(addrs: A) -> Result<(Socket<'static>, u16, SocketAddr), Error> {
        let rx_buffer = SocketBuffer::new(Vec::new());
        let tx_buffer = SocketBuffer::new(Vec::new());
        let mut sock: Socket<'static> = Socket::new(rx_buffer, tx_buffer); // this is the listening socket
        let (port, local_address) = {
            match Self::each_addr(addrs, &mut sock) {
                Ok((port, local_address)) => (port, local_address),
                Err(_) => return Err(Error::other("listening error")),
            }
        };
        Ok((sock, port, local_address))
    }
    /* bind
     * accepts: address(es)
     * returns: a tcpsocket
     * creates a tcpsocket and binds the address to that socket.
     * if multiple addresses given, it will attempt to bind to each until successful
     */
    /*
        example arguments passed to bind:
        "127.0.0.1:0"
        SocketAddr::from(([127, 0, 0, 1], 443))
        let addrs = [ SocketAddr::from(([127, 0, 0, 1], 80)),  SocketAddr::from(([127, 0, 0, 1], 443)), ];
    */
    pub fn bind<A: ToSocketAddrs>(addrs: A) -> Result<SmolTcpListener, Error> {
        let engine = &ENGINE;
        let (sock, port, local_address) = {
            match Self::do_bind(addrs) {
                Ok((sock, port, local_address)) => (sock, port, local_address),
                Err(_) => {
                    return Err(Error::other("listening error"));
                }
            }
        };
        println!("in bind");
        let handle = (*engine).add_socket(sock);
        // allocate a queue to hold pending connection requests. sounds like a semaphore
        let tcp = SmolTcpListener {
            socket_handle: handle,
            port,
            local_addr: local_address,
        };
        Ok(tcp)
    }

    // accept
    // block until there is a waiting connection in the queue
    // create a new socket for tcpstream
    // ^^ creating a new one so that the user can call accept() on the previous one again
    // return tcpstream
    pub fn accept(&self) -> Result<(SmolTcpStream, SocketAddr), Error> {
        // create another socket to listen on the same port and use that as a listener
        // we can have multiple sockets listening on the same port
        println!("in accept");
        // this is the listener
        let engine = &ENGINE;
        let stream;
        let mut socket: &mut Socket<'static>;
        loop {
            {
                let mut core = (*engine).core.lock().unwrap();
                core.poll(&engine.condvar);
            } // mutex drops here
            {
                let mut core = (*engine).core.lock().unwrap();
                socket = core.get_mutable_socket(self.socket_handle);
                if socket.is_active() {
                    let remote = socket.remote_endpoint().unwrap();
                    drop(core);
                    let _ = Self::bind(self.local_addr);
                    println!("accepted connection");
                    stream = SmolTcpStream {
                        socket_handle: self.socket_handle,
                        local_addr: self.local_addr,
                        port: self.port,
                    };
                    // the socket addr returned is that of the remote endpoint. ie. the client.
                    let remote_addr = SocketAddr::from((remote.addr, remote.port));
                    return Ok((stream, remote_addr));
                }
            } // mutex drops here
        }
        // how are we handling error cases? should there be some sort of a timeout?
        return Err(Error::other("accepting error"));
    }

    pub fn local_addr(&self) -> Result<SocketAddr, Error> {
        // rethink this one.
        // smoltcp supports fns listen_endpoint() and local_endpoint(). use one of those instead.
        return Ok(self.local_addr);
    }
}

#[derive(Debug)]
pub struct SmolTcpStream {
    socket_handle: SocketHandle,
    local_addr: SocketAddr,
    port: u16,
}
impl Read for SmolTcpStream {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        // check if the socket can even recieve
        //if may_recv(&self) {
        //    println!("Can recieve!");
        // call recv on up to the size of the buffer + load it
        // return recv's f
        //} else {
        //    println!("Cannot recieve :");
        //}
        todo!();
    }
}
impl Write for SmolTcpStream {
    // write
    fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
        // call can_send
        // call send on buffer, then return f from send
        todo!();
    }
    fn flush(&mut self) -> Result<(), Error> {
        // needs to make sure the output buffer is empty...
        //      maybe a loop of checking can_send until it's false?
        // have to check how the buffer is emptied. it seems automatic?
        todo!()
    }
}
pub trait From<SmolTcpStream> {
    fn new() {}
    fn from(s: SmolTcpStream) -> Self
    where
        Self: Sized,
    {
        todo!();
    }
}
impl From<SmolTcpStream> for SmolTcpStream {
    fn from(s: SmolTcpStream) -> SmolTcpStream {
        todo!();
    }
}

impl SmolTcpStream {
    /* each_addr:
     * helper function for connect()
     * processes each address given to see whether it can implement ToSocketAddr, then tries to
     * connect to that addr keeps trying each address until one of them successfully connects
     * parameters: addresses passed into connect(), reference to socket, reference to
     * interface context, and port.
     * return: port and address
     */
    fn each_addr<A: ToSocketAddrs>(
        sock_addrs: A,
        s: &mut Socket<'static>,
        cx: &mut Context,
        port: u16,
    ) -> Result<(), ConnectError> {
        let addrs = {
            match sock_addrs.to_socket_addrs() {
                Ok(addrs) => addrs,
                Err(e) => return Err(ConnectError::InvalidState),
            }
        };
        for addr in addrs {
            match (*s).connect(cx, addr, port) {
                Ok(_) => return Ok(()),
                Err(_) => return Err(ConnectError::Unaddressable),
            }
        }
        Err(ConnectError::InvalidState) // is that the correct thing to return?
    }
    /* connect():
     * parameters: address(es) a list of addresses may be given
     * return: a smoltcpstream that is connected to the remote server.
     */
    /// addr is an address of the remote host.
    pub fn connect<A: ToSocketAddrs>(addr: A) -> Result<SmolTcpStream, Error> {
        println!("in connect()");
        let engine = &ENGINE; // accessing global engine
        let mut sock = {
            // create new socket
            let rx_buffer = SocketBuffer::new(Vec::new());
            let tx_buffer = SocketBuffer::new(Vec::new());
            Socket::new(rx_buffer, tx_buffer)
        };
        // TODO: don't hardcode in port. make ephemeral port.
        let PORT = 49152;
        let config = Config::new(EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]).into()); // change later?
        let mut device = Loopback::new(Medium::Ethernet);
        let mut iface = Interface::new(config, &mut device, Instant::now());
        iface.update_ip_addrs(|ip_addrs| {
            ip_addrs
                .push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8))
                .unwrap();
        });
        if let Err(e) = Self::each_addr(addr, &mut sock, iface.context(), PORT) {
            println!("connect(): connection error!! {}", e);
            return Err(Error::other("connection error"));
        } else {
            // success
        }; // note to self: make sure remote endpoint matches the server address!
        let handle = (*engine).add_socket(sock);
        let smoltcpstream = SmolTcpStream {
            socket_handle: handle,
            port: PORT,
            local_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), PORT),
        };
        Ok(smoltcpstream)
    }

    /* peer_addr():
     * parameters: -
     * return: the remote address of the socket. this is the address of the server
     * note: can only be used if already connected
     */
    pub fn peer_addr(&self) -> Result<SocketAddr, Error> {
        let engine = &ENGINE;
        let mut core = (*engine).core.lock().unwrap();
        let socket = core.get_socket(self.socket_handle);
        let remote = socket.remote_endpoint().unwrap();
        drop(core);
        let remote_addr = SocketAddr::from((remote.addr, remote.port));
        Ok(remote_addr)
        // TODO: add error handling
    }

    /* shutdown_write():
     * helper function for shutdown()
     */
    fn shutdown_write(socket: &mut Socket<'static>) {
        socket.close(); // close() only closes the transmit half of the connection
    }
    /* shutdown_read():
     * helper function for shutdown()
     */
    fn shutdown_read(socket: &mut Socket<'static>) {
        socket.abort();
        // abort() immediately aborts the connection and closes the socket, sends a reset packet to
        // the remote endpoint
    }
    /* shutdown():
     * parameters: how - an enum of Shutdown that specifies what part of the socket to shutdown.
     *             options are Read, Write, or Both.
     * return: Result<> indicating success, (), or failure, Error
     */
    /* TODO: this really is an issue for later rather than sooner, but
    ASK DANIEL how he plans to handle this for Twizzler:

    "Calling this function multiple times may result in different behavior,
    depending on the operating system. On Linux, the second call will
    return `Ok(())`, but on macOS, it will return `ErrorKind::NotConnected`.
    This may change in the future." -- std::net documentation
    */
    pub fn shutdown(&self, how: Shutdown) -> Result<(), Error> {
        // specifies shutdown of read, write, or both with an enum.
        // write shutdown with close().
        // both with abort() though this will send a reset packet
        // TODO: what to do for read ?
        match how {
            Shutdown::Read => {}
            Shutdown::Write => {}
            Shutdown::Both => {}
        }
        todo!();
    }

    pub fn try_clone(&self) -> Result<SmolTcpStream, Error> {
        // use try_from on all of the contained elements?
        // more doc reading necessary
        todo!()
    }
}
// implement impl std::fmt::Debug for SmolTcpStream
// add `#[derive(Debug)]` to `SmolTcpStream` or manually `impl std::fmt::Debug for SmolTcpStream`

/*
tests:
make_listener:

*/
#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use crate::shim::SmolTcpListener;
    #[test]
    fn make_listener() {
        let listener = SmolTcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 443))).unwrap();
        let stream = listener.accept();
    }
}

use super::super::udp::*;
use super::super::Endpoint;

use log;

use std::convert::TryInto;
use std::io;
use std::mem;
use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};
use std::os::unix::io::RawFd;

fn errno() -> libc::c_int {
    unsafe {
        let ptr = libc::__errno_location();
        if ptr.is_null() {
            -1
        } else {
            *ptr
        }
    }
}

#[repr(C)]
struct ControlHeaderV4 {
    hdr: libc::cmsghdr,
    body: libc::in_pktinfo,
}

#[repr(C)]
struct ControlHeaderV6 {
    hdr: libc::cmsghdr,
    body: libc::in6_pktinfo,
}

pub struct EndpointV4 {
    dst: libc::sockaddr_in, // destination IP
    info: libc::in_pktinfo, // src & ifindex
}

pub struct EndpointV6 {
    dst: libc::sockaddr_in6, // destination IP
    info: libc::in6_pktinfo, // src & zone id
}

pub struct LinuxUDP();

pub struct LinuxOwner {
    port: u16,
    sock4: Option<RawFd>,
    sock6: Option<RawFd>,
}

pub enum LinuxUDPReader {
    V4(RawFd),
    V6(RawFd),
}

#[derive(Clone)]
pub struct LinuxUDPWriter {
    sock4: RawFd,
    sock6: RawFd,
}

pub enum LinuxEndpoint {
    V4(EndpointV4),
    V6(EndpointV6),
}

impl Endpoint for LinuxEndpoint {
    fn clear_src(&mut self) {
        match self {
            LinuxEndpoint::V4(EndpointV4 { ref mut info, .. }) => {
                info.ipi_ifindex = 0;
                info.ipi_spec_dst = libc::in_addr { s_addr: 0 };
            }
            LinuxEndpoint::V6(EndpointV6 { ref mut info, .. }) => {
                info.ipi6_addr = libc::in6_addr { s6_addr: [0; 16] };
                info.ipi6_ifindex = 0;
            }
        };
    }

    fn from_address(addr: SocketAddr) -> Self {
        match addr {
            SocketAddr::V4(addr) => LinuxEndpoint::V4(EndpointV4 {
                dst: libc::sockaddr_in {
                    sin_family: libc::AF_INET as libc::sa_family_t,
                    sin_port: addr.port(),
                    sin_addr: libc::in_addr {
                        s_addr: u32::from(*addr.ip()).to_be(),
                    },
                    sin_zero: [0; 8],
                },
                info: libc::in_pktinfo {
                    ipi_ifindex: 0,                            // interface (0 is via routing table)
                    ipi_spec_dst: libc::in_addr { s_addr: 0 }, // src IP (dst of incoming packet)
                    ipi_addr: libc::in_addr { s_addr: 0 },
                },
            }),
            SocketAddr::V6(addr) => LinuxEndpoint::V6(EndpointV6 {
                dst: libc::sockaddr_in6 {
                    sin6_family: libc::AF_INET6 as libc::sa_family_t,
                    sin6_port: addr.port(),
                    sin6_flowinfo: addr.flowinfo(),
                    sin6_addr: libc::in6_addr {
                        s6_addr: addr.ip().octets(),
                    },
                    sin6_scope_id: addr.scope_id(),
                },
                info: libc::in6_pktinfo {
                    ipi6_addr: libc::in6_addr { s6_addr: [0; 16] }, // src IP
                    ipi6_ifindex: 0,                                // zone id
                },
            }),
        }
    }

    fn into_address(&self) -> SocketAddr {
        match self {
            LinuxEndpoint::V4(EndpointV4 { ref dst, .. }) => {
                SocketAddr::V4(SocketAddrV4::new(
                    dst.sin_addr.s_addr.into(), // IPv4 addr
                    dst.sin_port,
                ))
            }
            LinuxEndpoint::V6(EndpointV6 { ref dst, .. }) => SocketAddr::V6(SocketAddrV6::new(
                u128::from_ne_bytes(dst.sin6_addr.s6_addr).into(), // IPv6 addr
                dst.sin6_port,
                dst.sin6_flowinfo,
                dst.sin6_scope_id,
            )),
        }
    }
}

fn setsockopt<V: Sized>(
    fd: RawFd,
    level: libc::c_int,
    name: libc::c_int,
    value: &V,
) -> Result<(), io::Error> {
    let res = unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            mem::transmute(value),
            mem::size_of_val(value).try_into().unwrap(),
        )
    };
    if res == 0 {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Other,
            "Failed to set sockopt",
        ))
    }
}

fn setsockopt_int(
    fd: RawFd,
    level: libc::c_int,
    name: libc::c_int,
    value: libc::c_int,
) -> Result<(), io::Error> {
    setsockopt(fd, level, name, &value)
}

impl LinuxUDPReader {
    fn read6(fd: RawFd, buf: &mut [u8]) -> Result<(usize, LinuxEndpoint), io::Error> {
        // this memory is mutated by the recvmsg call
        #[allow(unused_mut)]
        let mut control: ControlHeaderV6 = unsafe { mem::MaybeUninit::uninit().assume_init() };

        let iov = unsafe {
            libc::iovec {
                iov_base: mem::transmute(&buf[0] as *const u8),
                iov_len: buf.len(),
            }
        };

        let src: libc::sockaddr_in6 = unsafe { mem::MaybeUninit::uninit().assume_init() };

        let mut hdr = unsafe {
            libc::msghdr {
                msg_name: mem::transmute(&src),
                msg_namelen: mem::size_of_val(&src).try_into().unwrap(),
                msg_iov: mem::transmute(&iov),
                msg_iovlen: 1,
                msg_control: mem::transmute(&control),
                msg_controllen: mem::size_of_val(&control),
                msg_flags: 0, // ignored
            }
        };

        let len = unsafe { libc::recvmsg(fd, &mut hdr as *mut libc::msghdr, 0) };

        if len < 0 {
            // error
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "failed to send",
            ));
        }

        Ok((
            len.try_into().unwrap(),
            LinuxEndpoint::V6(EndpointV6 {
                info: control.body,
                dst: src,
            }),
        ))
    }

    fn read4(fd: RawFd, buf: &mut [u8]) -> Result<(usize, LinuxEndpoint), io::Error> {
        log::debug!("receive IPv4 packet (block), (fd {})", fd);

        // this memory is mutated by the recvmsg call
        #[allow(unused_mut)]
        let mut control: ControlHeaderV4 = unsafe { mem::MaybeUninit::uninit().assume_init() };

        let iovs: [libc::iovec; 1] = [unsafe {
            libc::iovec {
                iov_base: mem::transmute(&buf[0] as *const u8),
                iov_len: buf.len(),
            }
        }];

        let src: libc::sockaddr_in = unsafe { mem::MaybeUninit::uninit().assume_init() };

        let mut hdr = unsafe {
            libc::msghdr {
                msg_name: mem::transmute(&src),
                msg_namelen: mem::size_of_val(&src).try_into().unwrap(),
                msg_iov: mem::transmute(&iovs[0]),
                msg_iovlen: iovs.len(),
                msg_control: mem::transmute(&control),
                msg_controllen: mem::size_of_val(&control),
                msg_flags: 0, // ignored
            }
        };

        let len = unsafe { libc::recvmsg(fd, &mut hdr as *mut libc::msghdr, 0) };

        if len < 0 {
            // error
            log::debug!("failed to receive IPv4 packet (errno = {})", errno());
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "failed to send",
            ));
        }

        log::debug!("received IPv4 packet ({} fd, {} bytes)", fd, len);

        Ok((
            len.try_into().unwrap(),
            LinuxEndpoint::V4(EndpointV4 {
                info: control.body,
                dst: src,
            }),
        ))
    }
}

impl Reader<LinuxEndpoint> for LinuxUDPReader {
    type Error = io::Error;

    fn read(&self, buf: &mut [u8]) -> Result<(usize, LinuxEndpoint), Self::Error> {
        match self {
            Self::V4(fd) => Self::read4(*fd, buf),
            Self::V6(fd) => Self::read6(*fd, buf),
        }
    }
}

impl LinuxUDPWriter {
    fn write6(fd: RawFd, buf: &[u8], dst: &EndpointV6) -> Result<(), io::Error> {
        log::debug!("sending IPv6 packet ({} fd, {} bytes)", fd, buf.len());

        unimplemented!()
    }

    fn write4(fd: RawFd, buf: &[u8], dst: &EndpointV4) -> Result<(), io::Error> {
        log::debug!("sending IPv4 packet ({} fd, {} bytes)", fd, buf.len());

        let control = ControlHeaderV4 {
            hdr: libc::cmsghdr {
                cmsg_len: mem::size_of::<ControlHeaderV4>(),
                cmsg_level: libc::IPPROTO_IP,
                cmsg_type: libc::IP_PKTINFO,
            },
            body: dst.info,
        };

        let iovs: [libc::iovec; 1] = [unsafe {
            libc::iovec {
                iov_base: mem::transmute(&buf[0] as *const u8),
                iov_len: buf.len(),
            }
        }];

        let hdr = unsafe {
            libc::msghdr {
                msg_name: mem::transmute(&dst.dst),
                msg_namelen: mem::size_of_val(&dst.dst).try_into().unwrap(),
                msg_iov: mem::transmute(&iovs[0]),
                msg_iovlen: iovs.len(),
                msg_control: mem::transmute(&control),
                msg_controllen: 1,
                msg_flags: 0, // ignored
            }
        };

        let err = unsafe { libc::sendmsg(fd, &hdr, 0) };
        if err < 0 {
            log::debug!("failed to send IPv4: (errno = {})", errno());
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "failed to send IPv4 packet",
            ));
        }

        Ok(())
    }
}

impl Writer<LinuxEndpoint> for LinuxUDPWriter {
    type Error = io::Error;

    fn write(&self, buf: &[u8], dst: &LinuxEndpoint) -> Result<(), Self::Error> {
        match dst {
            LinuxEndpoint::V4(ref end) => Self::write4(self.sock4, buf, end),
            LinuxEndpoint::V6(ref end) => Self::write6(self.sock6, buf, end),
        }
    }
}

impl Owner for LinuxOwner {
    type Error = io::Error;

    fn get_port(&self) -> u16 {
        self.port
    }

    fn set_fwmark(&mut self, value: Option<u32>) -> Result<(), Self::Error> {
        fn set_mark(fd: Option<RawFd>, value: u32) -> Result<(), io::Error> {
            if let Some(fd) = fd {
                setsockopt(fd, libc::SOL_SOCKET, libc::SO_MARK, &value)
            } else {
                Ok(())
            }
        }
        let value = value.unwrap_or(0);
        set_mark(self.sock6, value)?;
        set_mark(self.sock4, value)
    }
}

impl Drop for LinuxOwner {
    fn drop(&mut self) {
        log::trace!("closing the bind (port {})", self.port);
        self.sock4.map(|fd| unsafe { libc::close(fd) });
        self.sock6.map(|fd| unsafe { libc::close(fd) });
    }
}

impl UDP for LinuxUDP {
    type Error = io::Error;
    type Endpoint = LinuxEndpoint;
    type Reader = LinuxUDPReader;
    type Writer = LinuxUDPWriter;
}

impl LinuxUDP {
    /* Bind on all interfaces with IPv6.
     *
     * Arguments:
     *
     * - 'port', port to bind to (0 = any)
     *
     * Returns:
     *
     * Returns a tuple of the resulting port and socket.
     */
    fn bind6(port: u16) -> Result<(u16, RawFd), io::Error> {
        log::debug!("attempting to bind on IPv6 (port {})", port);

        // create socket fd
        let fd: RawFd = unsafe { libc::socket(libc::AF_INET6, libc::SOCK_DGRAM, 0) };
        if fd < 0 {
            log::debug!("failed to create IPv6 socket");
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "failed to create socket",
            ));
        }

        setsockopt_int(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, 1)?;
        setsockopt_int(fd, libc::IPPROTO_IPV6, libc::IPV6_RECVPKTINFO, 1)?;
        setsockopt_int(fd, libc::IPPROTO_IPV6, libc::IPV6_V6ONLY, 1)?;

        // bind
        let mut sockaddr = libc::sockaddr_in6 {
            sin6_addr: libc::in6_addr { s6_addr: [0; 16] },
            sin6_family: libc::AF_INET6 as libc::sa_family_t,
            sin6_port: port.to_be(), // convert to network (big-endian) byteorder
            sin6_scope_id: 0,
            sin6_flowinfo: 0,
        };

        let err = unsafe {
            libc::bind(
                fd,
                mem::transmute(&sockaddr as *const libc::sockaddr_in6),
                mem::size_of_val(&sockaddr).try_into().unwrap(),
            )
        };

        if err != 0 {
            log::debug!("failed to bind IPv6 socket");
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "failed to create socket",
            ));
        }

        // get the assigned port
        let mut socklen: libc::socklen_t = 0;
        let err = unsafe {
            libc::getsockname(
                fd,
                mem::transmute(&mut sockaddr as *mut libc::sockaddr_in6),
                &mut socklen as *mut libc::socklen_t,
            )
        };
        if err != 0 {
            log::debug!("failed to get port of IPv6 socket");
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "failed to create socket",
            ));
        }

        // basic sanity checks
        let new_port = u16::from_be(sockaddr.sin6_port);
        debug_assert_eq!(socklen, mem::size_of::<libc::sockaddr_in6>() as u32);
        debug_assert_eq!(sockaddr.sin6_family, libc::AF_INET6 as libc::sa_family_t);
        debug_assert_eq!(new_port, if port != 0 { port } else { new_port });
        return Ok((new_port, fd));
    }

    /* Bind on all interfaces with IPv4.
     *
     * Arguments:
     *
     * - 'port', port to bind to (0 = any)
     *
     * Returns:
     *
     * Returns a tuple of the resulting port and socket.
     */
    fn bind4(port: u16) -> Result<(u16, RawFd), io::Error> {
        log::trace!("attempting to bind on IPv4 (port {})", port);

        // create socket fd
        let fd: RawFd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        if fd < 0 {
            log::trace!("failed to create IPv4 socket (errno = {})", errno());
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "failed to create socket",
            ));
        }

        setsockopt_int(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, 1)?;
        setsockopt_int(fd, libc::IPPROTO_IP, libc::IP_PKTINFO, 1)?;

        // bind
        let mut sockaddr = libc::sockaddr_in {
            sin_addr: libc::in_addr { s_addr: 0 },
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: port.to_be(), // convert to network (big-endian) byteorder
            sin_zero: [0; 8],
        };

        let err = unsafe {
            libc::bind(
                fd,
                mem::transmute(&sockaddr as *const libc::sockaddr_in),
                mem::size_of_val(&sockaddr).try_into().unwrap(),
            )
        };

        if err != 0 {
            log::trace!("failed to bind IPv4 socket (errno = {})", errno());
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "failed to create socket",
            ));
        }

        // get the assigned port
        let mut socklen: libc::socklen_t = 0;
        let err = unsafe {
            libc::getsockname(
                fd,
                mem::transmute(&mut sockaddr as *mut libc::sockaddr_in),
                &mut socklen as *mut libc::socklen_t,
            )
        };
        if err != 0 {
            log::trace!("failed to get port of IPv4 socket (errno = {})", errno());
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "failed to create socket",
            ));
        }

        // basic sanity checks
        let new_port = u16::from_be(sockaddr.sin_port);
        debug_assert_eq!(socklen, mem::size_of::<libc::sockaddr_in>() as u32);
        debug_assert_eq!(sockaddr.sin_family, libc::AF_INET as libc::sa_family_t);
        debug_assert_eq!(new_port, if port != 0 { port } else { new_port });
        log::debug!("bound IPv4 socket on port {}", new_port);
        return Ok((new_port, fd));
    }
}

impl PlatformUDP for LinuxUDP {
    type Owner = LinuxOwner;

    fn bind(mut port: u16) -> Result<(Vec<Self::Reader>, Self::Writer, Self::Owner), Self::Error> {
        log::debug!("bind to port {}", port);

        // attempt to bind on ipv6
        let bind6 = Self::bind6(port);
        if let Ok((new_port, _)) = bind6 {
            port = new_port;
        }

        // attempt to bind on ipv4 on the same port
        let bind4 = Self::bind4(port);
        if let Ok((new_port, _)) = bind4 {
            port = new_port;
        }

        // check if failed to bind on both
        if bind4.is_err() && bind6.is_err() {
            log::trace!("failed to bind for either IP version");
            return Err(bind6.unwrap_err());
        }

        let sock6 = bind6.ok().map(|(_, fd)| fd);
        let sock4 = bind4.ok().map(|(_, fd)| fd);

        // create owner
        let owner = LinuxOwner {
            port,
            sock6: sock6,
            sock4: sock4,
        };

        // create readers
        let mut readers: Vec<Self::Reader> = Vec::with_capacity(2);
        sock6.map(|sock| readers.push(LinuxUDPReader::V6(sock)));
        sock4.map(|sock| readers.push(LinuxUDPReader::V4(sock)));
        debug_assert!(readers.len() > 0);

        // create writer
        let writer = LinuxUDPWriter {
            sock4: sock4.unwrap_or(-1),
            sock6: sock6.unwrap_or(-1),
        };

        Ok((readers, writer, owner))
    }
}

use std::os::unix::io::RawFd;
use std::ptr::{null, null_mut};
use std::str::FromStr;
use libc::{c_int, size_t, ssize_t, in_addr, in6_addr, write, c_void, c_char};
use log::error;
use nix::sys::socket::{Ipv4Addr, Ipv6Addr};
use nix::sys::time::TimeValLike;
use nix::time::{clock_gettime, ClockId};
use crate::net::bindings::{Slirp, slirp_cleanup, slirp_new, SlirpCb, SlirpConfig, SlirpTimerCb};

// Strongly based on https://github.com/rootless-containers/slirp4netns/blob/master/slirp4netns.c

struct Timer {
    cb: SlirpTimerCb,
    cb_opaque: *const c_void,
    expire_timer_msec: i64
}

struct UserData<'x> {
    tapfd: RawFd,
    timers: Vec<&'x Timer>
}

unsafe extern "C" fn send_packet(pkt: *const c_void, pkt_len: size_t, opaque: *mut c_void) -> ssize_t
{
    let data = opaque as *mut UserData;
    return write((*data).tapfd, pkt, pkt_len);
}

unsafe extern "C" fn guest_error(msg: *const c_char, _opaque: *mut c_void)
{
    error!("libslirp: {:?}", msg);
}

unsafe extern "C" fn clock_get_ns(_opaque: *mut c_void) -> i64
{
    clock_gettime(ClockId::CLOCK_MONOTONIC).unwrap().num_nanoseconds()
}

unsafe extern "C" fn timer_new(cb: SlirpTimerCb, cb_opaque: *mut c_void, opaque: *mut c_void) -> *mut c_void
{
    let data = opaque as *mut UserData;
    let t = Timer {
        cb,
        expire_timer_msec: -1,
        cb_opaque
    };

    (*data).timers.push(&t);

    // return std::ptr::addr_of_mut!((*data).timers.last_mut().unwrap());

    return std::ptr::addr_of!(t) as *mut c_void;
}

unsafe extern "C" fn timer_free(timer: *mut c_void, opaque: *mut c_void)
{
    let data = opaque as *mut UserData;
    let timer_obj = timer as *const &Timer;
    (*data).timers.retain(|t| {
        !std::ptr::eq(t, timer_obj)
    })
}

unsafe extern "C" fn timer_mod(timer: *mut c_void, expire_timer_msec: i64, _opaque: *mut c_void)
{
    let data = timer as *mut Timer;
    (*data).expire_timer_msec = expire_timer_msec
}

extern "C" fn register_poll_fd(_fd: c_int, _opaque: *mut c_void)
{
/*
 * NOP
 *
 * This is NOP on QEMU@4c76137484878f42a2ce1ae1b888b6a7f66b4053 on Linux as
 * well, see:
 *  * qemu/net/slirp.c:          net_slirp_register_poll_fd (calls
 * qemu_fd_register)
 *  * qemu/stubs/fd-register.c:  qemu_fd_register (NOP on Linux)
 *
 *  See also:
 *  * qemu/util/main-loop.c:     qemu_fd_register (Win32 only)
 */
}

extern "C" fn unregister_poll_fd(_fd: i32, _opaque: *mut c_void)
{
/*
 * NOP
 *
 * This is NOP on QEMU@4c76137484878f42a2ce1ae1b888b6a7f66b4053 as well,
 * see:
 *  * qemu/net/slirp.c:          net_slirp_unregister_poll_fd (NOP)
 */
}

extern "C" fn notify(_opaque: *mut c_void)
{
/*
 * NOP
 *
 * This can be NOP on QEMU@4c76137484878f42a2ce1ae1b888b6a7f66b4053 as well,
 * see:
 *  * qemu/net/slirp.c:          net_slirp_notify (calls qemu_notify_event)
 *  * qemu/stubs/notify-event.c: qemu_notify_event (NOP)
 *
 *  See also:
 *  * qemu/util/main-loop.c:     qemu_notify_event (NOP if
 * !qemu_aio_context)
 */
}

pub struct MyIpv6Addr {
    addr: Ipv6Addr
}

impl From<&str> for MyIpv6Addr {
    fn from(s: &str) -> Self {
        Self {
            addr: Ipv6Addr::from_std(&std::net::Ipv6Addr::from_str(s).unwrap())
        }
    }
}
impl From<MyIpv6Addr> for in6_addr {
    fn from(addr: MyIpv6Addr) -> Self {
        addr.addr.0
    }
}

pub struct MyIpv4Addr {
    addr: Ipv4Addr
}

impl From<&str> for MyIpv4Addr {
    fn from(s: &str) -> Self {
        Self {
            addr: Ipv4Addr::from_std(&std::net::Ipv4Addr::from_str(s).unwrap())
        }
    }
}
impl From<MyIpv4Addr> for in_addr {
    fn from(addr: MyIpv4Addr) -> Self {
        addr.addr.0
    }
}


pub fn init() -> *mut Slirp {
    let ipv6 = true;
    let ipv4 = true;
    let mtu = 0;
    let disable_host_loopback = false;
    let disable_dns = false;

    let cb = SlirpCb {
        send_packet,
        guest_error,
        clock_get_ns,
        timer_new: Some(timer_new),
        timer_free: Some(timer_free),
        timer_mod: Some(timer_mod),
        register_poll_fd,
        unregister_poll_fd,
        notify
    };
    // 192.168.0.0 - network
    // 192.168.0.1 - gateway
    // 192.168.0.2 - vhost
    // 192.168.0.3 - dns
    // 192.168.0.15 - dhcp (+15)
    // #define DEFAULT_RECOMMENDED_VGUEST_OFFSET (100) // 10.0.2.100
    // #define NETWORK_PREFIX_MIN (1)
    // >=26 is not supported because the recommended guest IP is set to network addr
    // + 100 .
    // #define NETWORK_PREFIX_MAX (25)
    let config = SlirpConfig {
        version: 3,
        restricted: 0,
        in_enabled: ipv4,
        vnetwork: MyIpv4Addr::from("192.168.10.0").into(),
        vnetmask: MyIpv4Addr::from("255.255.255.0").into(),
        vhost: MyIpv4Addr::from("192.168.10.2").into(),
        in6_enabled: ipv6,
        vprefix_addr6: MyIpv6Addr::from("fd00::").into(),
        vprefix_len: 64,
        vhost6: MyIpv6Addr::from("fd00::2").into(),
        vhostname: null(),
        tftp_server_name: null(),
        tftp_path: null(),
        bootfile: null(),
        vdhcp_start: MyIpv4Addr::from("192.168.10.15").into(),
        vnameserver: MyIpv4Addr::from("192.168.10.3").into(),
        vnameserver6: MyIpv6Addr::from("fd00::3").into(),
        vdnssearch: null_mut(),
        vdomainname: null(),
        if_mtu: mtu,
        if_mru: mtu,
        disable_host_loopback,
        // if config ver >= 2
        outbound_addr: null_mut(),
        outbound_addr6: null_mut(),
        // if config ver >= 3
        disable_dns,
        enable_emu: false,
    };
    let mut data = UserData {
        tapfd: 0,
        timers: Vec::new()
    };

    return unsafe { slirp_new(&config, &cb, std::ptr::addr_of_mut!(data) as *mut c_void) };
}

pub fn cleanup(handle: *mut Slirp) {
    // TODO: free UserData
    unsafe { slirp_cleanup(handle) }
}

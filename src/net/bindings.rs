use libc::{c_char, c_int, size_t, ssize_t, c_void, socklen_t, in_addr, sockaddr, sockaddr_in, sockaddr_in6, in6_addr};

// copy SLIPR init/usage from: https://github.com/rootless-containers/slirp4netns/blob/master/slirp4netns.c

// RFC 1861
pub enum Slirp {}

type SlirpReadCb = unsafe extern "C" fn(
    buf: *mut c_void,
    len: size_t,
    opaque: *mut c_void,
) -> ssize_t;

type SlirpWriteCb = unsafe extern "C" fn(
    buf: *const c_void,
    len: size_t,
    opaque: *mut c_void,
) -> ssize_t;

pub type SlirpTimerCb = unsafe extern "C" fn(opaque: *mut c_void);

type SlirpAddPollCb = unsafe extern "C" fn(
    fd: c_int,
    events: c_int,
    opaque: *mut c_void,
) -> c_int;

type SlirpGetREventsCb = unsafe extern "C" fn(
    idx: c_int,
    opaque: *mut c_void,
) -> c_int;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct SlirpCb {
    pub send_packet: SlirpWriteCb,
    pub guest_error: unsafe extern "C" fn(
        msg: *const c_char,
        opaque_ptr: *mut c_void,
    ),
    pub clock_get_ns: unsafe extern "C" fn(opaque: *mut c_void) -> i64,
    pub timer_new: Option<
        unsafe extern "C" fn(
            cb: SlirpTimerCb,
            cb_opaque_ptr: *const c_void,
            opaque_ptr: *mut c_void,
        ) -> *const c_void,
    >,
    pub timer_free: Option<
        unsafe extern "C" fn(
            timer: *mut c_void,
            opaque: *mut c_void,
        ),
    >,
    pub timer_mod: Option<
        unsafe extern "C" fn(
            timer_ptr: *mut c_void,
            expire_time: i64,
            opaque_ptr: *mut c_void,
        ),
    >,
    pub register_poll_fd: unsafe extern "C" fn(fd: c_int, opaque_ptr: *mut c_void),
    pub unregister_poll_fd: unsafe extern "C" fn(fd: c_int, opaque_ptr: *mut c_void),
    pub notify: unsafe extern "C" fn(opaque_ptr: *mut c_void),
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct SlirpConfig {
    pub version: u32,
    pub restricted: c_int,
    pub in_enabled: bool,
    pub vnetwork: in_addr,
    pub vnetmask: in_addr,
    pub vhost: in_addr,
    pub in6_enabled: bool,
    pub vprefix_addr6: in6_addr,
    pub vprefix_len: u8,
    pub vhost6: in6_addr,
    pub vhostname: *const c_char,
    pub tftp_server_name: *const c_char,
    pub tftp_path: *const c_char,
    pub bootfile: *const c_char,
    pub vdhcp_start: in_addr,
    pub vnameserver: in_addr,
    pub vnameserver6: in6_addr,
    pub vdnssearch: *mut *const c_char,
    pub vdomainname: *const c_char,
    pub if_mtu: size_t,
    pub if_mru: size_t,
    pub disable_host_loopback: bool,
    pub enable_emu: bool,
    pub outbound_addr: *mut sockaddr_in,
    pub outbound_addr6: *mut sockaddr_in6,
    pub disable_dns: bool,
}

extern "C" {
    pub fn slirp_new(
        cfg: *const SlirpConfig,
        callbacks: *const SlirpCb,
        opaque: *const c_void,
    ) -> *const Slirp;
    pub fn slirp_cleanup(slirp: *mut Slirp);
    pub fn slirp_pollfds_fill(
        slirp: *mut Slirp,
        timeout: *mut u32,
        add_poll: SlirpAddPollCb,
        opaque: *mut c_void,
    );
    pub fn slirp_pollfds_poll(
        slirp: *mut Slirp,
        select_error: c_int,
        get_revents: SlirpGetREventsCb,
        opaque: *mut c_void,
    );
    pub fn slirp_input(slirp: *mut Slirp, pkt: *const u8, pkt_len: c_int);
    pub fn slirp_add_hostfwd(
        slirp: *mut Slirp,
        is_udp: c_int,
        host_addr: in_addr,
        host_port: c_int,
        guest_addr: in_addr,
        guest_port: c_int,
    ) -> c_int;
    pub fn slirp_remove_hostfwd(
        slirp: *mut Slirp,
        is_udp: c_int,
        host_addr: in_addr,
        host_port: c_int,
    ) -> c_int;
    pub fn slirp_add_hostxfwd(
        slirp: *mut Slirp,
        haddr: *const sockaddr,
        haddrlen: socklen_t,
        gaddr: *const sockaddr,
        gaddrlen: socklen_t,
        flags: c_int,
    ) -> c_int;
    pub fn slirp_remove_hostxfwd(
        slirp: *mut Slirp,
        haddr: *const sockaddr,
        haddrlen: socklen_t,
        flags: c_int,
    ) -> c_int;
    pub fn slirp_add_exec(
        slirp: *mut Slirp,
        cmdline: *const c_char,
        guest_addr: *mut in_addr,
        guest_port: c_int,
    ) -> c_int;
    pub fn slirp_add_unix(
        slirp: *mut Slirp,
        unixsock: *const c_char,
        guest_addr: *mut in_addr,
        guest_port: c_int,
    ) -> c_int;
    pub fn slirp_add_guestfwd(
        slirp: *mut Slirp,
        write_cb: Option<SlirpWriteCb>,
        opaque: *mut c_void,
        guest_addr: *mut in_addr,
        guest_port: c_int,
    ) -> c_int;
    pub fn slirp_socket_can_recv(
        slirp: *mut Slirp,
        guest_addr: in_addr,
        guest_port: c_int,
    ) -> size_t;
    pub fn slirp_socket_recv(
        slirp: *mut Slirp,
        guest_addr: in_addr,
        guest_port: c_int,
        buf: *const u8,
        size: c_int,
    );
    pub fn slirp_remove_guestfwd(
        slirp: *mut Slirp,
        guest_addr: in_addr,
        guest_port: c_int,
    ) -> c_int;
    pub fn slirp_connection_info(slirp: *mut Slirp) -> *mut c_char;
    pub fn slirp_neighbor_info(slirp: *mut Slirp) -> *mut c_char;
    pub fn slirp_state_save(
        s: *mut Slirp,
        write_cb: SlirpWriteCb,
        opaque: *mut c_void,
    );
    pub fn slirp_state_version() -> c_int;
    pub fn slirp_state_load(
        s: *mut Slirp,
        version_id: c_int,
        read_cb: SlirpReadCb,
        opaque: *mut c_void,
    ) -> c_int;
    pub fn slirp_version_string() -> *const c_char;
}

//! DRM lease fd receiver for Game Mode.
//!
//! Connects to Gamescope's Unix domain socket and receives a DRM lease fd
//! via SCM_RIGHTS (sendmsg/recvmsg ancillary data). The lease fd is a DRM
//! master for the leased connector/CRTC/plane — the companion app can
//! perform mode-setting and page-flips on it directly.
//!
//! The Unix stream is kept open for the entire process lifetime and used
//! by Gamescope as a liveness signal: while we're connected, Gamescope
//! drops touch events for the bottom-screen touchscreen (we own them).
//! When this process exits the socket closes, Gamescope sees POLLHUP, and
//! touch events start flowing to wlserver again (needed for Desktop Mode).

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

use anyhow::{bail, Context, Result};

/// Default socket path. Override with `--lease-socket` CLI flag or
/// `GAMESCOPE_LEASE_SOCK` environment variable.
pub const DEFAULT_SOCKET_PATH: &str = "/tmp/gamescope-lease.sock";

/// Result of receiving the lease: the DRM master fd plus the liveness
/// socket that must be kept alive for the entire process lifetime.
pub struct LeaseConnection {
    pub lease_fd: OwnedFd,
    /// Hold on to this for the whole process lifetime. Dropping it closes
    /// the Unix socket connection, which signals Gamescope that the
    /// companion has exited.
    pub _liveness: UnixStream,
}

/// Connect to Gamescope's lease socket and receive the DRM lease fd.
///
/// The protocol is trivial: connect, receive exactly one byte of data ('L')
/// plus one SCM_RIGHTS fd. The returned `UnixStream` must be held open for
/// the process lifetime so Gamescope can detect companion exit.
pub fn receive_lease_fd(socket_path: &str) -> Result<LeaseConnection> {
    let stream = UnixStream::connect(socket_path)
        .with_context(|| format!("connect to '{socket_path}'"))?;

    let lease_fd = recv_fd(stream.as_raw_fd())?;
    Ok(LeaseConnection {
        lease_fd,
        _liveness: stream,
    })
}

/// Low-level recvmsg with SCM_RIGHTS to extract a single file descriptor.
fn recv_fd(sock: i32) -> Result<OwnedFd> {
    // One byte of regular data (Gamescope sends 'L').
    let mut data_buf = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: data_buf.as_mut_ptr().cast(),
        iov_len: 1,
    };

    // Control message buffer — 64 bytes is generous for one fd.
    let mut cmsg_buf = [0u8; 64];

    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = &mut iov;
    hdr.msg_iovlen = 1;
    hdr.msg_control = cmsg_buf.as_mut_ptr().cast();
    hdr.msg_controllen = cmsg_buf.len() as _;

    let n = unsafe { libc::recvmsg(sock, &mut hdr, 0) };
    if n < 0 {
        bail!("recvmsg: {}", std::io::Error::last_os_error());
    }
    if n == 0 {
        bail!("connection closed before receiving fd");
    }

    let cmsg = unsafe { libc::CMSG_FIRSTHDR(&hdr) };
    if cmsg.is_null() {
        bail!("no control message in response");
    }

    unsafe {
        if (*cmsg).cmsg_level != libc::SOL_SOCKET || (*cmsg).cmsg_type != libc::SCM_RIGHTS {
            bail!(
                "unexpected cmsg type {}/{}",
                (*cmsg).cmsg_level,
                (*cmsg).cmsg_type
            );
        }
        let lease_fd = *(libc::CMSG_DATA(cmsg) as *const i32);
        if lease_fd < 0 {
            bail!("received invalid fd: {lease_fd}");
        }
        Ok(OwnedFd::from_raw_fd(lease_fd))
    }
}

/// Query the DRM version string on the lease fd to verify it's a real
/// DRM master. Returns the driver name (e.g. "amdgpu") or an error.
pub fn verify_drm_fd(fd: &OwnedFd) -> Result<String> {
    // DRM_IOCTL_VERSION = DRM_IOWR(0x00, struct drm_version)
    // We call libdrm's drmGetVersion via libc FFI.
    //
    // For simplicity we use the raw ioctl number. The struct layout is
    // stable ABI. We only read the name field.

    #[repr(C)]
    struct DrmVersion {
        version_major: i32,
        version_minor: i32,
        version_patchlevel: i32,
        name_len: usize,
        name: *mut u8,
        date_len: usize,
        date: *mut u8,
        desc_len: usize,
        desc: *mut u8,
    }

    // First call with NULL pointers to get lengths.
    let mut ver: DrmVersion = unsafe { std::mem::zeroed() };

    // DRM_IOCTL_VERSION = 0xC0406400 on LP64 (varies by arch, but the
    // _IOWR macro produces this). Safer: compute at runtime.
    const DRM_IOCTL_BASE: u64 = b'd' as u64;
    // _IOWR('d', 0x00, struct drm_version)
    // direction = 0xC0 (read+write), size = size_of::<DrmVersion>()
    let size = std::mem::size_of::<DrmVersion>() as u64;
    let ioctl_nr: u64 = (0xC0 << 24) | (size << 16) | (DRM_IOCTL_BASE << 8) | 0x00;

    let raw = fd.as_raw_fd();
    let ret = unsafe { libc::ioctl(raw, ioctl_nr as libc::c_ulong, &mut ver) };
    if ret < 0 {
        bail!("DRM_IOCTL_VERSION (probe): {}", std::io::Error::last_os_error());
    }

    if ver.name_len == 0 {
        bail!("DRM driver name is empty");
    }

    // Second call with a buffer for the name.
    let mut name_buf = vec![0u8; ver.name_len + 1];
    ver.name = name_buf.as_mut_ptr();

    let ret = unsafe { libc::ioctl(raw, ioctl_nr as libc::c_ulong, &mut ver) };
    if ret < 0 {
        bail!("DRM_IOCTL_VERSION (read): {}", std::io::Error::last_os_error());
    }

    let name = String::from_utf8_lossy(&name_buf[..ver.name_len]).to_string();
    Ok(name)
}

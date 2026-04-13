use std::collections::VecDeque;
use std::env;
use std::io;
use std::mem;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use io_uring::{IoUring, opcode, types::Fd};
use tracing::{debug, info};

pub struct Connection {
    socket: UnixStream,
    ring: IoUring,
    write_buf: Vec<u8>,
    next_id: u32,
    pending_fds: VecDeque<OwnedFd>,
    recycled_ids: Vec<u32>,
}

impl Connection {
    pub fn connect() -> io::Result<Self> {
        let path = Self::resolve_socket_path()?;
        info!(path = ?path, "connecting to Wayland socket");
        Self::connect_to(&path)
    }

    pub fn connect_to(path: &Path) -> io::Result<Self> {
        let socket = UnixStream::connect(path)?;
        let ring = IoUring::new(8).map_err(io::Error::other)?;
        Ok(Connection {
            socket,
            ring,
            write_buf: Vec::with_capacity(4096),
            next_id: 2,
            pending_fds: VecDeque::new(),
            recycled_ids: Vec::new(),
        })
    }

    fn resolve_socket_path() -> io::Result<PathBuf> {
        let runtime_dir = env::var("XDG_RUNTIME_DIR")
            .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR not set"))?;
        let display = env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".to_string());

        if display.starts_with('/') {
            Ok(PathBuf::from(display))
        } else {
            let mut path = PathBuf::from(runtime_dir);
            path.push(display);
            Ok(path)
        }
    }

    pub fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        debug!(id, "allocated object id");
        id
    }

    pub fn send_msg(&mut self, object_id: u32, opcode: u16, args: &[u8]) -> io::Result<()> {
        crate::wire::send_msg(&mut self.write_buf, object_id, opcode, args)
    }

    pub fn flush(&mut self) -> io::Result<()> {
        if self.write_buf.is_empty() {
            return Ok(());
        }

        let fd = Fd(self.socket.as_raw_fd());
        let mut offset = 0usize;
        let total = self.write_buf.len();

        while offset < total {
            let buf = &self.write_buf[offset..];
            // SAFETY: write_buf is heap-allocated and lives until submit_and_wait returns.
            // offset(u64::MAX) signals stream semantics (equivalent to write(2)) for sockets.
            let write_e = opcode::Write::new(fd, buf.as_ptr(), buf.len() as u32)
                .offset(u64::MAX)
                .build();
            unsafe { self.ring.submission().push(&write_e) }
                .map_err(|_| io::Error::other("submission queue full"))?;
            self.ring.submit_and_wait(1)?;

            let cqe = self
                .ring
                .completion()
                .next()
                .ok_or_else(|| io::Error::other("no completion entry"))?;
            let n = cqe.result();
            if n < 0 {
                return Err(io::Error::from_raw_os_error(-n));
            }
            offset += n as usize;
        }

        tracing::trace!(bytes = total, "→ flush");
        self.write_buf.clear();
        Ok(())
    }

    pub fn send_msg_with_fds(
        &mut self,
        object_id: u32,
        opcode: u16,
        args: &[u8],
        fds: Vec<OwnedFd>,
    ) -> io::Result<()> {
        self.flush()?;

        let raw_fds: Vec<RawFd> = fds.iter().map(|f| f.as_raw_fd()).collect();

        let total_size = 8 + args.len();
        let word2 = ((total_size as u32) << 16) | (opcode as u32);
        let mut payload = Vec::with_capacity(total_size);
        payload.extend_from_slice(&object_id.to_le_bytes());
        payload.extend_from_slice(&word2.to_le_bytes());
        payload.extend_from_slice(args);

        // Allocate cmsg buffer for SCM_RIGHTS.
        let cmsg_space =
            unsafe { libc::CMSG_SPACE(std::mem::size_of_val(raw_fds.as_slice()) as libc::c_uint) }
                as usize;
        let mut cmsg_buf = vec![0u8; cmsg_space];

        let iov = libc::iovec {
            iov_base: payload.as_ptr() as *mut libc::c_void,
            iov_len: payload.len(),
        };

        let mut mhdr: libc::msghdr = unsafe { mem::zeroed() };
        mhdr.msg_iov = &iov as *const libc::iovec as *mut libc::iovec;
        mhdr.msg_iovlen = 1;
        mhdr.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        mhdr.msg_controllen = cmsg_buf.len() as _;

        // Populate SCM_RIGHTS control message.
        // SAFETY: cmsg_buf is sized via CMSG_SPACE; CMSG_FIRSTHDR/CMSG_DATA follow POSIX layout.
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&mhdr);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len =
                libc::CMSG_LEN(std::mem::size_of_val(raw_fds.as_slice()) as libc::c_uint) as _;
            let data = libc::CMSG_DATA(cmsg) as *mut RawFd;
            std::ptr::copy_nonoverlapping(raw_fds.as_ptr(), data, raw_fds.len());
        }

        tracing::trace!(
            object_id,
            opcode,
            bytes = total_size,
            fds = raw_fds.len(),
            "→ sendmsg"
        );

        // SAFETY: payload, iov, cmsg_buf, and mhdr all live in this stack frame.
        // submit_and_wait(1) returns only after the kernel completes the operation.
        let send_e =
            opcode::SendMsg::new(Fd(self.socket.as_raw_fd()), &mhdr as *const libc::msghdr).build();
        unsafe { self.ring.submission().push(&send_e) }
            .map_err(|_| std::io::Error::other("submission queue full"))?;
        self.ring.submit_and_wait(1)?;

        let cqe = self
            .ring
            .completion()
            .next()
            .ok_or_else(|| std::io::Error::other("no completion entry"))?;
        let n = cqe.result();
        if n < 0 {
            return Err(io::Error::from_raw_os_error(-n));
        }
        Ok(())
    }

    pub fn recv_msg(&mut self) -> io::Result<(u32, u16, Vec<u8>)> {
        // Phase 1: RECVMSG for 8-byte header + any ancillary fds.
        let mut header_buf = [0u8; 8];
        let mut cmsg_space_buf = nix::cmsg_space!([RawFd; 28]);

        let iov = libc::iovec {
            iov_base: header_buf.as_mut_ptr() as *mut libc::c_void,
            iov_len: 8,
        };
        let mut mhdr: libc::msghdr = unsafe { mem::zeroed() };
        mhdr.msg_iov = &iov as *const libc::iovec as *mut libc::iovec;
        mhdr.msg_iovlen = 1;
        mhdr.msg_control = cmsg_space_buf.as_mut_ptr() as *mut libc::c_void;
        mhdr.msg_controllen = cmsg_space_buf.len() as _;

        // SAFETY: header_buf, cmsg_space_buf, iov, mhdr all live in this stack frame.
        let recv_e =
            opcode::RecvMsg::new(Fd(self.socket.as_raw_fd()), &mut mhdr as *mut libc::msghdr)
                .build();
        unsafe { self.ring.submission().push(&recv_e) }
            .map_err(|_| io::Error::other("submission queue full"))?;
        self.ring.submit_and_wait(1)?;

        let cqe = self
            .ring
            .completion()
            .next()
            .ok_or_else(|| io::Error::other("no completion entry"))?;
        let n = cqe.result();
        if n < 0 {
            return Err(io::Error::from_raw_os_error(-n));
        }

        // Extract any received fds from cmsg.
        // SAFETY: kernel updated mhdr.msg_controllen; CMSG_FIRSTHDR/NXTHDR follow POSIX layout.
        // from_raw_fd takes ownership of each fd the kernel passed us.
        unsafe {
            let mut cmsg_ptr = libc::CMSG_FIRSTHDR(&mhdr);
            while !cmsg_ptr.is_null() {
                if (*cmsg_ptr).cmsg_level == libc::SOL_SOCKET
                    && (*cmsg_ptr).cmsg_type == libc::SCM_RIGHTS
                {
                    let data_ptr = libc::CMSG_DATA(cmsg_ptr) as *const RawFd;
                    let fd_count = ((*cmsg_ptr).cmsg_len as usize
                        - mem::size_of::<libc::cmsghdr>())
                        / mem::size_of::<RawFd>();
                    for i in 0..fd_count {
                        let fd = *data_ptr.add(i);
                        self.pending_fds.push_back(OwnedFd::from_raw_fd(fd));
                    }
                }
                cmsg_ptr = libc::CMSG_NXTHDR(&mhdr, cmsg_ptr);
            }
        }

        // Decode header fields.
        let object_id = u32::from_le_bytes(header_buf[0..4].try_into().unwrap());
        let word2 = u32::from_le_bytes(header_buf[4..8].try_into().unwrap());
        let total_len = (word2 >> 16) as usize;
        let opcode = (word2 & 0xffff) as u16;
        let body_len = total_len - 8;

        // Phase 2: READ the body bytes.
        let mut body = vec![0u8; body_len];
        if body_len > 0 {
            // SAFETY: body is heap-allocated; submit_and_wait(1) is synchronous.
            let read_e = opcode::Read::new(
                Fd(self.socket.as_raw_fd()),
                body.as_mut_ptr(),
                body_len as u32,
            )
            .offset(u64::MAX)
            .build();
            unsafe { self.ring.submission().push(&read_e) }
                .map_err(|_| io::Error::other("submission queue full"))?;
            self.ring.submit_and_wait(1)?;

            let cqe = self
                .ring
                .completion()
                .next()
                .ok_or_else(|| io::Error::other("no completion entry"))?;
            let n = cqe.result();
            if n < 0 {
                return Err(io::Error::from_raw_os_error(-n));
            }
        }

        tracing::trace!(object_id, opcode, bytes = total_len, "← recvmsg");
        Ok((object_id, opcode, body))
    }

    /// Non-blocking recv: returns `Ok(None)` immediately if no data is waiting.
    /// Uses a MSG_DONTWAIT|MSG_PEEK check before delegating to `recv_msg`.
    pub fn try_recv_msg(&mut self) -> io::Result<Option<(u32, u16, Vec<u8>)>> {
        let mut byte = 0u8;
        let n = unsafe {
            libc::recv(
                self.socket.as_raw_fd(),
                &mut byte as *mut u8 as *mut libc::c_void,
                1,
                libc::MSG_DONTWAIT | libc::MSG_PEEK,
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::WouldBlock {
                Ok(None)
            } else {
                Err(e)
            };
        }
        self.recv_msg().map(Some)
    }

    pub fn pop_fd(&mut self) -> io::Result<OwnedFd> {
        self.pending_fds
            .pop_front()
            .ok_or_else(|| io::Error::other("Error"))
    }
}

// SAFETY: `Connection` is used exclusively from one thread at a time.
// `UnixStream` and `IoUring` are both safe to move between threads; the
// io_uring ring's kernel-mapped memory is process-global and thread-agnostic.
unsafe impl Send for Connection {}

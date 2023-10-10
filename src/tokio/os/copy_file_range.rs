use super::{SyscallAvailability, INVALID_FD};
use crate::{cvt, try_libc};

use cfg_if::cfg_if;
use displaydoc::Display;
use libc;
use once_cell::sync::Lazy;
use tokio::task;
use tokio_pipe::{PipeRead, PipeWrite};

use std::{
    io, mem,
    os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd},
    pin::Pin,
    ptr,
};

fn invalid_copy_file_range() -> io::Error {
    let ret = unsafe {
        libc::copy_file_range(
            INVALID_FD,
            ptr::null_mut(),
            INVALID_FD,
            ptr::null_mut(),
            1,
            0,
        )
    };
    assert_eq!(-1, ret);
    io::Error::last_os_error()
}

pub static HAS_COPY_FILE_RANGE: Lazy<SyscallAvailability> = Lazy::new(|| {
    cfg_if! {
        if #[cfg(target_os = "linux")] {
            match invalid_copy_file_range().raw_os_error().unwrap() {
                libc::EBADF => SyscallAvailability::Available,
                errno => SyscallAvailability::FailedProbe(io::Error::from_raw_os_error(errno)),
            }
        } else {
            SyscallAvailability::NotOnThisPlatform
        }
    }
});

pub struct RawArgs<'a> {
    fd: libc::c_int,
    off: Option<&'a mut libc::off64_t>,
}

pub trait CopyFileRangeHandle {
    fn role(&self) -> Role;
    fn as_args(self: Pin<&mut Self>) -> RawArgs<'_>;
}

pub struct MutateInnerOffset {
    pub role: Role,
    pub owned_fd: OwnedFd,
}

impl MutateInnerOffset {
    pub fn new(f: impl IntoRawFd, role: Role) -> io::Result<Self> {
        let raw_fd = validate_raw_fd(f.into_raw_fd(), role)?;
        let owned_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        Ok(Self { role, owned_fd })
    }

    pub fn into_owned(self) -> OwnedFd {
        self.owned_fd
    }
}

impl CopyFileRangeHandle for MutateInnerOffset {
    fn role(&self) -> Role {
        self.role
    }
    fn as_args(self: Pin<&mut Self>) -> RawArgs<'_> {
        RawArgs {
            fd: self.owned_fd.as_raw_fd(),
            off: None,
        }
    }
}

pub struct FromGivenOffset {
    fd: RawFd,
    pub offset: i64,
    role: Role,
}

impl FromGivenOffset {
    pub fn new(f: &impl AsRawFd, role: Role, init: u32) -> io::Result<Self> {
        let raw_fd = f.as_raw_fd();
        let fd = validate_raw_fd(raw_fd, role)?;
        Ok(Self {
            fd,
            role,
            offset: init as i64,
        })
    }
}

impl AsRawFd for FromGivenOffset {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl CopyFileRangeHandle for FromGivenOffset {
    fn role(&self) -> Role {
        self.role
    }
    fn as_args(self: Pin<&mut Self>) -> RawArgs {
        let Self {
            fd, ref mut offset, ..
        } = self.get_mut();
        RawArgs {
            fd: fd.as_raw_fd(),
            off: Some(offset),
        }
    }
}

#[inline]
fn convert_option_ptr<T>(mut p: Option<&mut T>) -> *mut T {
    if let Some(ref mut val) = p {
        &mut **val
    } else {
        ptr::null_mut()
    }
}

pub async fn iter_copy_file_range(
    src: Pin<&mut impl CopyFileRangeHandle>,
    dst: Pin<&mut impl CopyFileRangeHandle>,
    len: usize,
) -> io::Result<usize> {
    assert_eq!(src.role(), Role::Readable);
    let RawArgs {
        fd: fd_in,
        off: off_in,
    } = src.as_args();
    let off_in = convert_option_ptr(off_in);
    let off_in = off_in as usize;

    assert_eq!(dst.role(), Role::Writable);
    let RawArgs {
        fd: fd_out,
        off: off_out,
    } = dst.as_args();
    let off_out = convert_option_ptr(off_out);
    let off_out = off_out as usize;

    /* These must always be set to 0 for now. */
    const FUTURE_FLAGS: libc::c_uint = 0;
    let written: libc::ssize_t = task::spawn_blocking(move || {
        let off_in = off_in as *mut libc::off64_t;
        let off_out = off_out as *mut libc::off64_t;
        cvt!(unsafe { libc::copy_file_range(fd_in, off_in, fd_out, off_out, len, FUTURE_FLAGS) })
    })
    .await
    .unwrap()?;
    assert!(written >= 0);
    Ok(written as usize)
}

pub async fn iter_splice_from_pipe(
    mut src: Pin<&mut PipeRead>,
    dst: Pin<&mut impl CopyFileRangeHandle>,
    len: usize,
) -> io::Result<usize> {
    assert_eq!(dst.role(), Role::Writable);
    let RawArgs {
        fd: fd_out,
        off: off_out,
    } = dst.as_args();

    src.splice_to_blocking_fd(fd_out, off_out, len, false).await
}

pub async fn splice_from_pipe(
    mut src: Pin<&mut PipeRead>,
    mut dst: Pin<&mut impl CopyFileRangeHandle>,
    full_len: usize,
) -> io::Result<usize> {
    let mut remaining = full_len;

    while remaining > 0 {
        let cur_written = iter_splice_from_pipe(src.as_mut(), dst.as_mut(), remaining).await?;
        assert!(cur_written <= remaining);
        if cur_written == 0 {
            return Ok(full_len - remaining);
        }
        remaining -= cur_written;
    }
    Ok(full_len)
}

pub async fn iter_splice_to_pipe(
    src: Pin<&mut impl CopyFileRangeHandle>,
    mut dst: Pin<&mut PipeWrite>,
    len: usize,
) -> io::Result<usize> {
    assert_eq!(src.role(), Role::Readable);
    let RawArgs {
        fd: fd_in,
        off: off_in,
    } = src.as_args();

    dst.splice_from_blocking_fd(fd_in, off_in, len).await
}

pub async fn splice_to_pipe(
    mut src: Pin<&mut impl CopyFileRangeHandle>,
    mut dst: Pin<&mut PipeWrite>,
    full_len: usize,
) -> io::Result<usize> {
    let mut remaining = full_len;

    while remaining > 0 {
        let cur_written = iter_splice_to_pipe(src.as_mut(), dst.as_mut(), remaining).await?;
        assert!(cur_written <= remaining);
        if cur_written == 0 {
            return Ok(full_len - remaining);
        }
        remaining -= cur_written;
    }
    Ok(full_len)
}

pub async fn copy_file_range(
    mut src: Pin<&mut impl CopyFileRangeHandle>,
    mut dst: Pin<&mut impl CopyFileRangeHandle>,
    full_len: usize,
) -> io::Result<usize> {
    let mut remaining = full_len;

    while remaining > 0 {
        let cur_written = iter_copy_file_range(src.as_mut(), dst.as_mut(), remaining).await?;
        assert!(cur_written <= remaining);
        if cur_written == 0 {
            return Ok(full_len - remaining);
        }
        remaining -= cur_written;
    }
    Ok(full_len)
}

fn check_regular_file(fd: RawFd) -> io::Result<()> {
    let mut stat = mem::MaybeUninit::<libc::stat>::uninit();

    try_libc!(unsafe { libc::fstat(fd, stat.as_mut_ptr()) });

    let stat = unsafe { stat.assume_init() };
    if (stat.st_mode & libc::S_IFMT) == libc::S_IFREG {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Other,
            "Fd is not a regular file",
        ))
    }
}

fn get_status_flags(fd: RawFd) -> io::Result<libc::c_int> {
    Ok(try_libc!(unsafe { libc::fcntl(fd, libc::F_GETFL) }))
}

#[derive(Copy, Clone, Debug, Display, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum Role {
    /// fd has the read capability
    Readable,
    /// fd has the write capability
    Writable,
}

impl Role {
    fn allowed_modes(&self) -> &'static [libc::c_int] {
        static READABLE: &'static [libc::c_int] = &[libc::O_RDONLY, libc::O_RDWR];
        static WRITABLE: &'static [libc::c_int] = &[libc::O_WRONLY, libc::O_RDWR];
        match self {
            Self::Readable => READABLE,
            Self::Writable => WRITABLE,
        }
    }

    fn check_append(&self, flags: libc::c_int) -> io::Result<()> {
        if let Self::Writable = self {
            if (flags & libc::O_APPEND) != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Writable Fd was set for append!",
                ));
            }
        }
        Ok(())
    }

    fn errmsg(&self) -> &'static str {
        static READABLE: &'static str = "Fd is not readable!";
        static WRITABLE: &'static str = "Fd is not writable!";
        match self {
            Self::Readable => READABLE,
            Self::Writable => WRITABLE,
        }
    }

    pub(crate) fn validate_flags(&self, flags: libc::c_int) -> io::Result<()> {
        let access_mode = flags & libc::O_ACCMODE;

        if !self.allowed_modes().contains(&access_mode) {
            return Err(io::Error::new(io::ErrorKind::Other, self.errmsg()));
        }
        self.check_append(flags)?;

        Ok(())
    }
}

fn validate_raw_fd(fd: RawFd, role: Role) -> io::Result<RawFd> {
    check_regular_file(fd)?;

    let status_flags = get_status_flags(fd)?;
    role.validate_flags(status_flags)?;

    Ok(fd)
}

#[cfg(test)]
mod test {
    use super::*;

    use std::fs;

    #[test]
    fn check_copy_file_range() {
        assert!(matches!(
            *HAS_COPY_FILE_RANGE,
            SyscallAvailability::Available
        ));
    }

    #[test]
    fn check_readable_writable_file() {
        let f = tempfile::tempfile().unwrap();
        let fd: RawFd = f.as_raw_fd();

        validate_raw_fd(fd, Role::Readable).unwrap();
        validate_raw_fd(fd, Role::Writable).unwrap();
    }

    #[test]
    fn check_only_writable() {
        let td = tempfile::tempdir().unwrap();
        let f = fs::OpenOptions::new()
            .create_new(true)
            .read(false)
            .write(true)
            .open(td.path().join("asdf.txt"))
            .unwrap();
        let fd: RawFd = f.as_raw_fd();

        validate_raw_fd(fd, Role::Writable).unwrap();
        assert!(validate_raw_fd(fd, Role::Readable).is_err());
    }

    #[test]
    fn check_only_readable() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("asdf.txt");
        fs::write(&p, b"wow!").unwrap();

        let f = fs::OpenOptions::new()
            .read(true)
            .write(false)
            .open(&p)
            .unwrap();
        let fd: RawFd = f.as_raw_fd();

        validate_raw_fd(fd, Role::Readable).unwrap();
        assert!(validate_raw_fd(fd, Role::Writable).is_err());
    }

    #[test]
    fn check_no_append() {
        let td = tempfile::tempdir().unwrap();

        let f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .write(true)
            .open(td.path().join("asdf.txt"))
            .unwrap();
        let fd: RawFd = f.as_raw_fd();

        assert!(validate_raw_fd(fd, Role::Writable).is_err());
        assert!(validate_raw_fd(fd, Role::Readable).is_err());
    }

    #[tokio::test]
    async fn read_ref_into_write_owned() {
        use io::{Read, Seek};

        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("asdf.txt");
        fs::write(&p, b"wow!").unwrap();

        let in_file = fs::File::open(&p).unwrap();
        let mut src = FromGivenOffset::new(&in_file, Role::Readable, 0).unwrap();
        let sp = Pin::new(&mut src);

        let p2 = td.path().join("asdf2.txt");
        let out_file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            /* Need this to read the output file contents at the end! */
            .read(true)
            .open(&p2)
            .unwrap();
        let mut dst = MutateInnerOffset::new(out_file, Role::Writable).unwrap();
        let dp = Pin::new(&mut dst);

        /* Explicit offset begins at 0. */
        assert_eq!(0, sp.offset);

        /* 4 bytes were written. */
        assert_eq!(
            4,
            /* NB: 5 bytes were requested! */
            copy_file_range(sp, dp, 5).await.unwrap()
        );
        assert_eq!(4, src.offset);

        let mut dst: fs::File = dst.into_owned().into();
        assert_eq!(4, dst.stream_position().unwrap());
        dst.rewind().unwrap();
        let mut s = String::new();
        dst.read_to_string(&mut s).unwrap();
        assert_eq!(&s, "wow!");
    }

    #[tokio::test]
    async fn test_splice_blocking() {
        use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

        let mut in_file = tokio::fs::File::from_std(tempfile::tempfile().unwrap());
        in_file.write_all(b"hello").await.unwrap();
        in_file.rewind().await.unwrap();
        let mut in_file = MutateInnerOffset::new(in_file.into_std().await, Role::Readable).unwrap();

        let mut out_file = tokio::fs::File::from_std(tempfile::tempfile().unwrap());
        let mut out_file_handle = FromGivenOffset::new(&out_file, Role::Writable, 0).unwrap();

        let (mut r, mut w) = tokio_pipe::pipe().unwrap();

        let w_task = tokio::spawn(async move {
            splice_to_pipe(Pin::new(&mut in_file), Pin::new(&mut w), 5)
                .await
                .unwrap();
        });

        let r_task = tokio::spawn(async move {
            splice_from_pipe(Pin::new(&mut r), Pin::new(&mut out_file_handle), 5)
                .await
                .unwrap();
            assert_eq!(out_file_handle.offset, 5);
        });

        tokio::try_join!(w_task, r_task).unwrap();

        assert_eq!(0, out_file.stream_position().await.unwrap());
        let mut s = String::new();
        out_file.read_to_string(&mut s).await.unwrap();
        assert_eq!(&s, "hello");
    }
}

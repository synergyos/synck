//! Loaded into a real `synck` process: SIGKILL at the Nth filesystem mutation.
//!
//! `SYNCK_CRASH_AT=N` picks the mutation. The call does not run, so the disk holds
//! exactly the first N-1 mutations. Writes to the standard streams do not count.

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::sync::atomic::{AtomicI64, Ordering::SeqCst};

const SIGKILL: c_int = 9;
const UNREAD: i64 = i64::MIN;

unsafe extern "C" {
    fn atoll(text: *const c_char) -> i64;
    fn getenv(name: *const c_char) -> *const c_char;
    fn getpid() -> c_int;
    fn kill(pid: c_int, signal: c_int) -> c_int;
}

static REMAINING: AtomicI64 = AtomicI64::new(UNREAD);

fn crash_point() {
    if REMAINING.load(SeqCst) == UNREAD {
        let value = unsafe { getenv(c"SYNCK_CRASH_AT".as_ptr()) };
        let target = if value.is_null() { i64::MAX } else { unsafe { atoll(value) } };
        let _ = REMAINING.compare_exchange(UNREAD, target, SeqCst, SeqCst);
    }
    if REMAINING.fetch_sub(1, SeqCst) == 1 {
        loop {
            unsafe { kill(getpid(), SIGKILL) };
        }
    }
}

fn file_write(fd: c_int) {
    if fd > 2 {
        crash_point();
    }
}

#[cfg(target_os = "macos")]
#[repr(transparent)]
struct Pointer(*const c_void);
#[cfg(target_os = "macos")]
unsafe impl Sync for Pointer {}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
}

macro_rules! mutation {
    ($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty, $point:expr) => {
        #[cfg(target_os = "macos")]
        mod $name {
            use super::*;
            unsafe extern "C" {
                fn $name($($arg: $ty),*) -> $ret;
            }
            unsafe extern "C" fn counted($($arg: $ty),*) -> $ret {
                $point;
                unsafe { $name($($arg),*) }
            }
            #[used]
            #[unsafe(link_section = "__DATA,__interpose")]
            static ENTRY: [Pointer; 2] =
                [Pointer(counted as *const c_void), Pointer($name as *const c_void)];
        }
        #[cfg(target_os = "linux")]
        mod $name {
            use super::*;
            #[unsafe(no_mangle)]
            unsafe extern "C" fn $name($($arg: $ty),*) -> $ret {
                $point;
                let name = concat!(stringify!($name), "\0");
                let real = unsafe { dlsym(-1isize as *mut c_void, name.as_ptr().cast()) };
                let real: unsafe extern "C" fn($($ty),*) -> $ret =
                    unsafe { std::mem::transmute(real) };
                unsafe { real($($arg),*) }
            }
        }
    };
}

mutation!(write(fd: c_int, data: *const c_void, size: usize) -> isize, file_write(fd));
mutation!(pwrite(fd: c_int, data: *const c_void, size: usize, at: i64) -> isize, file_write(fd));
mutation!(writev(fd: c_int, data: *const c_void, count: c_int) -> isize, file_write(fd));
mutation!(ftruncate(fd: c_int, size: i64) -> c_int, file_write(fd));
mutation!(rename(from: *const c_char, to: *const c_char) -> c_int, crash_point());
mutation!(
    renameat(from_dir: c_int, from: *const c_char, to_dir: c_int, to: *const c_char) -> c_int,
    crash_point()
);
mutation!(unlink(path: *const c_char) -> c_int, crash_point());
mutation!(unlinkat(dir: c_int, path: *const c_char, flags: c_int) -> c_int, crash_point());
mutation!(mkdir(path: *const c_char, mode: c_uint) -> c_int, crash_point());
mutation!(mkdirat(dir: c_int, path: *const c_char, mode: c_uint) -> c_int, crash_point());
mutation!(rmdir(path: *const c_char) -> c_int, crash_point());
mutation!(
    linkat(from_dir: c_int, from: *const c_char, to_dir: c_int, to: *const c_char, flags: c_int) -> c_int,
    crash_point()
);
#[cfg(target_os = "macos")]
mutation!(
    renameatx_np(from_dir: c_int, from: *const c_char, to_dir: c_int, to: *const c_char, flags: c_uint) -> c_int,
    crash_point()
);
#[cfg(target_os = "linux")]
mutation!(pwrite64(fd: c_int, data: *const c_void, size: usize, at: i64) -> isize, file_write(fd));

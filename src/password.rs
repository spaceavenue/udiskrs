use std::io::{BufRead, Error, IsTerminal, Result, Write};
use std::os::fd::AsRawFd;

#[repr(C)]
#[derive(Clone, Copy, Default)]
#[allow(clippy::struct_field_names)]
struct Termios {
  c_iflag: u32,
  c_oflag: u32,
  c_cflag: u32,
  c_lflag: u32,
  c_line: u8,
  c_cc: [u8; 32],
  c_ispeed: u32,
  c_ospeed: u32,
}

const ECHO: u32 = 0x0008;
const TCSAFLUSH: std::ffi::c_int = 2;

unsafe extern "C" {
  fn tcgetattr(fd: std::ffi::c_int, termios_p: *mut Termios) -> std::ffi::c_int;
  fn tcsetattr(
    fd: std::ffi::c_int,
    optional_actions: std::ffi::c_int,
    termios_p: *const Termios,
  ) -> std::ffi::c_int;
}

struct EchoGuard {
  fd: std::ffi::c_int,
  orig: Termios,
  active: bool,
}
impl EchoGuard {
  fn disable(fd: std::ffi::c_int) -> Result<Self> {
    let mut orig = Termios::default();
    if unsafe { tcgetattr(fd, &raw mut orig) } != 0 {
      return Err(Error::last_os_error());
    }

    let mut raw = orig;
    raw.c_lflag &= !ECHO;

    if unsafe { tcsetattr(fd, TCSAFLUSH, &raw const raw) } != 0 {
      return Err(Error::last_os_error());
    }

    Ok(Self {
      fd,
      orig,
      active: true,
    })
  }
}
impl Drop for EchoGuard {
  fn drop(&mut self) {
    if self.active {
      unsafe {
        tcsetattr(self.fd, TCSAFLUSH, &raw const self.orig);
      }
    }
  }
}

pub fn prompt_password(prompt: &str) -> Result<String> {
  let stdin = std::io::stdin();
  let is_tty = IsTerminal::is_terminal(&stdin);

  print!("{prompt}");
  Write::flush(&mut std::io::stdout())?;

  let guard = if is_tty {
    Some(EchoGuard::disable(stdin.as_raw_fd())?)
  } else {
    None
  };

  let mut password = String::new();
  let res = std::io::stdin().lock().read_line(&mut password);

  drop(guard);

  if is_tty {
    println!();
  }

  res?;

  if password.ends_with('\n') {
    password.pop();
    if password.ends_with('\r') {
      password.pop();
    }
  }

  Ok(password)
}

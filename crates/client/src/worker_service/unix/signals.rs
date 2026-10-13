use expri_core::error::Result;
use std::sync::atomic::{AtomicBool, Ordering};
pub(super) static SIGNAL_STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn signal_stop(_: libc::c_int) {
  SIGNAL_STOP.store(true, Ordering::Release);
}
pub(super) struct SignalGuard {
  interrupt: libc::sigaction,
  terminate: libc::sigaction,
}
impl SignalGuard {
  pub(super) fn install() -> Result<Self> {
    SIGNAL_STOP.store(false, Ordering::Release);
    unsafe {
      let mut action: libc::sigaction = std::mem::zeroed();
      action.sa_sigaction = signal_stop as *const () as usize;
      libc::sigemptyset(&mut action.sa_mask);
      let mut interrupt = std::mem::zeroed();
      let mut terminate = std::mem::zeroed();
      if libc::sigaction(libc::SIGINT, &action, &mut interrupt) != 0 {
        return Err(std::io::Error::last_os_error().into());
      }
      if libc::sigaction(libc::SIGTERM, &action, &mut terminate) != 0 {
        libc::sigaction(libc::SIGINT, &interrupt, std::ptr::null_mut());
        return Err(std::io::Error::last_os_error().into());
      }
      Ok(Self {
        interrupt,
        terminate,
      })
    }
  }
}
impl Drop for SignalGuard {
  fn drop(&mut self) {
    unsafe {
      libc::sigaction(libc::SIGINT, &self.interrupt, std::ptr::null_mut());
      libc::sigaction(libc::SIGTERM, &self.terminate, std::ptr::null_mut());
    }
  }
}

//! Reap the supervised process even when session setup fails before `drive`.

use std::io;
use std::ops::{Deref, DerefMut};

pub struct ChildGuard {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    reaped: bool,
}

impl ChildGuard {
    pub fn new(child: Box<dyn portable_pty::Child + Send + Sync>) -> Self {
        Self {
            child,
            reaped: false,
        }
    }

    pub fn reap(&mut self) -> io::Result<()> {
        self.child.wait()?;
        self.reaped = true;
        Ok(())
    }
}

impl Deref for ChildGuard {
    type Target = dyn portable_pty::Child + Send + Sync;
    fn deref(&self) -> &Self::Target {
        &*self.child
    }
}

impl DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self.child
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        // try_wait also handles a child already reaped by drive's exit check.
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        if self.child.kill().is_ok() {
            let _ = self.child.wait();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn an_early_setup_return_kills_and_reaps_the_real_pty_child() {
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize::default())
            .unwrap();
        let mut command = portable_pty::CommandBuilder::new("sleep");
        command.arg("60");
        let child = ChildGuard::new(pair.slave.spawn_command(command).unwrap());
        let pid = child.process_id().unwrap() as libc::pid_t;
        drop(child);
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "PTY child survived setup failure"
        );
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }
}

//! Keeps the computer from sleeping during a call.

/// Held for the duration of the call; dropping it lets the system sleep again.
pub struct Awake(#[allow(dead_code)] Box<dyn Send>);

/// Best effort: if the system refuses, the call just goes on without it.
pub fn keep_awake(reason: &str) -> Option<Awake> {
    imp::keep_awake(reason).map(|h| Awake(Box::new(h)))
}

#[cfg(target_os = "linux")]
mod imp {
    pub fn keep_awake(reason: &str) -> Option<impl Send + 'static> {
        // On a plain thread: zbus's blocking API panics on one that drives a tokio runtime
        // (where the call actor runs).
        let reason = reason.to_string();
        std::thread::spawn(move || inhibit(&reason)).join().ok().flatten()
    }

    fn inhibit(reason: &str) -> Option<std::os::fd::OwnedFd> {
        let bus = zbus::blocking::Connection::system().ok()?;
        let manager = zbus::blocking::Proxy::new(&bus, "org.freedesktop.login1",
            "/org/freedesktop/login1", "org.freedesktop.login1.Manager").ok()?;
        let fd: zbus::zvariant::OwnedFd = manager.call("Inhibit", &("sleep:idle", "Crabber", reason, "block")).ok()?;
        Some(std::os::fd::OwnedFd::from(fd))
    }
}

#[cfg(windows)]
mod imp {
    use std::sync::mpsc;
    use std::thread::JoinHandle;
    use windows::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED};

    struct Guard {
        stop: mpsc::Sender<()>,
        thread: Option<JoinHandle<()>>,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.stop.send(());
            if let Some(thread) = self.thread.take() { let _ = thread.join(); }
        }
    }

    pub fn keep_awake(_: &str) -> Option<impl Send + 'static> {
        let (stop, done) = mpsc::channel();
        let (ready, result) = mpsc::channel();
        let thread = std::thread::Builder::new().name("keep-awake".into()).spawn(move || {
            // Execution state belongs to this thread, including the reset.
            let state = unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED) };
            let _ = ready.send(state.0 != 0);
            if state.0 == 0 { return; }
            let _ = done.recv();
            unsafe { SetThreadExecutionState(ES_CONTINUOUS); }
        }).ok()?;
        let guard = Guard { stop, thread: Some(thread) };
        result.recv().ok()?.then_some(guard)
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    pub fn keep_awake(_: &str) -> Option<()> {
        None
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    /// The call actor asks from a tokio task: that must not panic (it killed every later call).
    #[tokio::test(flavor = "multi_thread")]
    async fn safe_inside_tokio() {
        let held = tokio::spawn(async { super::keep_awake("Crabber test").is_some() }).await;
        assert!(held.is_ok(), "keep_awake panicked inside tokio");
    }

    #[test]
    #[ignore = "requires systemd-logind and permission to inhibit sleep"]
    fn logind_inhibitor_is_released() {
        let reason = format!("Crabber inhibitor test {}", std::process::id());
        let guard = super::keep_awake(&reason).expect("logind refused inhibitor");
        let list = || {
            let output = std::process::Command::new("systemd-inhibit")
                .args(["--list", "--no-pager"]).env("LC_ALL", "C").output().unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout).unwrap()
        };
        let held = list();
        println!("{held}");
        assert!(held.contains(&reason));
        drop(guard);
        assert!(!list().contains(&reason));
    }
}


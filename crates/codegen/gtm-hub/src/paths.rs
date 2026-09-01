use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const SOCKET_ENV: &str = "GTM_HUB_SOCKET";
pub const HOME_ENV: &str = "GTM_HOME";

/// Serializes tests that mutate `GTM_HOME` (process-global).
/// Guard is `Send` so async tests can hold it across `.await`.
#[cfg(test)]
pub struct TestHomeGuard;

#[cfg(test)]
impl Drop for TestHomeGuard {
    fn drop(&mut self) {
        test_home_release();
    }
}

#[cfg(test)]
fn test_home_flag() -> &'static std::sync::atomic::AtomicBool {
    use std::sync::atomic::AtomicBool;
    use std::sync::OnceLock;
    static FLAG: OnceLock<AtomicBool> = OnceLock::new();
    FLAG.get_or_init(|| AtomicBool::new(false))
}

#[cfg(test)]
pub fn lock_test_home() -> TestHomeGuard {
    use std::sync::atomic::Ordering;
    let flag = test_home_flag();
    while flag
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    TestHomeGuard
}

#[cfg(test)]
fn test_home_release() {
    use std::sync::atomic::Ordering;
    test_home_flag().store(false, Ordering::SeqCst);
}

pub fn gtm_home() -> PathBuf {
    if let Some(home) = std::env::var_os(HOME_ENV).filter(|v| !v.is_empty()) {
        return PathBuf::from(home);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".gtm")
}

pub fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os(SOCKET_ENV).filter(|v| !v.is_empty()) {
        return PathBuf::from(path);
    }
    gtm_home().join("hub.sock")
}

pub fn lock_path() -> PathBuf {
    socket_path()
        .parent()
        .map(|p| p.join("hub.lock"))
        .unwrap_or_else(|| gtm_home().join("hub.lock"))
}

pub fn log_path() -> PathBuf {
    gtm_home().join("hub.log")
}

pub fn ca_dir() -> PathBuf {
    gtm_home().join("ca")
}

pub fn ca_cert_path() -> PathBuf {
    ca_dir().join("ca.pem")
}

pub fn ca_key_path() -> PathBuf {
    ca_dir().join("ca.key")
}

pub fn tls_dir() -> PathBuf {
    gtm_home().join("tls")
}

pub fn tls_cert_path() -> PathBuf {
    tls_dir().join("server.pem")
}

pub fn tls_key_path() -> PathBuf {
    tls_dir().join("server.key")
}

pub fn devices_dir() -> PathBuf {
    gtm_home().join("devices")
}

pub fn ensure_home() -> std::io::Result<PathBuf> {
    let home = gtm_home();
    fs::create_dir_all(&home)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&home, fs::Permissions::from_mode(0o700));
    }
    Ok(home)
}

pub fn read_pid() -> Option<u32> {
    let mut buf = String::new();
    fs::File::open(lock_path())
        .ok()?
        .read_to_string(&mut buf)
        .ok()?;
    buf.trim().parse().ok()
}

#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(not(unix))]
pub fn pid_alive(_pid: u32) -> bool {
    false
}

pub fn cleanup_stale() {
    let _ = fs::remove_file(socket_path());
    let _ = fs::remove_file(lock_path());
}

pub fn is_stale_socket(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    #[cfg(unix)]
    {
        std::os::unix::net::UnixStream::connect(path).is_err()
    }
    #[cfg(not(unix))]
    {
        true
    }
}

use std::process::Command;

/// A stack overflow aborts, so each small-stack probe needs its own process.
pub(crate) fn small_stack(probe: fn()) {
    let current = std::thread::current();
    let name = current.name().expect("named test thread");
    const FLAG: &str = "FIGMOG_DEEP_JSON_PROBE";
    if std::env::var(FLAG).as_deref() == Ok(name) {
        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(probe)
            .unwrap()
            .join()
            .unwrap();
        return;
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(FLAG, name)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "probe {name}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

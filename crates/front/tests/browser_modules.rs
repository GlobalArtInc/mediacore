use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

const SUITE: &str = "tests/browser-modules.test.mjs";
const LIMIT: Duration = Duration::from_secs(60);

#[test]
fn the_pages_own_modules_do_what_they_say() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let suite = root.join(SUITE);
    assert!(suite.is_file(), "{} is missing", suite.display());

    let mut child = match Command::new("node")
        .arg("--test")
        .arg(&suite)
        .current_dir(&root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => panic!(
            "node could not be run ({error}), and the page's javascript is not covered without \
             it. Install node, or run `node --test crates/front/{SUITE}` yourself."
        ),
    };

    let started = Instant::now();
    loop {
        match child.try_wait().expect("wait for node") {
            Some(_) => break,
            None if started.elapsed() > LIMIT => {
                let _ = child.kill();
                panic!("node did not finish {SUITE} within {LIMIT:?}");
            }
            None => sleep(Duration::from_millis(20)),
        }
    }

    let output = child.wait_with_output().expect("collect node output");
    assert!(
        output.status.success(),
        "the page's own modules failed their tests:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

use std::env;
use std::os::raw::c_int;
use std::thread;
use std::time::Duration;

#[cfg(unix)]
extern "C" {
    fn getppid() -> c_int;
}

pub fn exit_when_parent_dies_if_requested() {
    if env::var("MACEVERY_EXIT_WITH_PARENT").ok().as_deref() != Some("1") {
        return;
    }

    #[cfg(unix)]
    thread::spawn(|| loop {
        if unsafe { getppid() } <= 1 {
            std::process::exit(0);
        }
        thread::sleep(Duration::from_secs(2));
    });
}

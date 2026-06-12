fn main() {
    if let Err(err) = macevery::cli::run(std::env::args()) {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

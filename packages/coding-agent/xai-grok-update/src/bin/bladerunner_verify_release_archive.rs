//! Release CI helper for validating Bladerunner archives, bundled assets, and checksums.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(error) = xai_grok_update::run_verify_release_cli(&args) {
        eprintln!("bladerunner-verify-release-archive: error: {error:#}");
        std::process::exit(1);
    }
}

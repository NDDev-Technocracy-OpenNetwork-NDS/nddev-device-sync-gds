#[cfg(target_os = "linux")]
#[path = "../worker/entrypoint.rs"]
mod entrypoint;
#[cfg(target_os = "linux")]
#[path = "../worker/reader.rs"]
mod reader;
fn main() {
    #[cfg(target_os = "linux")]
    entrypoint::run();
    #[cfg(not(target_os = "linux"))]
    std::process::exit(13);
}

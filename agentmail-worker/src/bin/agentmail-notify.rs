#[cfg(target_os = "linux")]
#[path = "agentmail-notify/service.rs"]
mod service;

#[cfg(target_os = "linux")]
fn main() -> std::io::Result<()> {
    service::run()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("agentmail-notify runs on the Linux mail VM");
    std::process::exit(1);
}

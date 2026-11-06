// Windows: a GUI app, so double-clicking doesn't open a console window too.
// --version and --help attach to the console they were started from.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod shares_cli;

slint::include_modules!();

const HELP: &str = "\
Tailgate: manage Tailscale and its Mullvad exit nodes

USAGE
  tailgate [--read-only] [--login]

FLAGS
  --read-only   Show live status, but refuse every change before it runs
  --login       Open minimized (what Open Tailgate at login starts)
  --shares      Look for SMB file shares on the tailnet, list what's
                connected, and exit (read-only)
  --connect-share //[user@]host/Share
                Connect one share the way the Shares tab does, and exit
  --disconnect-share //[user@]host/Share
                Disconnect it, and exit
  --where       Print which tailscale command Tailgate would use, ask it for
                status (read-only), and exit
  --version     Print the version
  --help        Print this

ENVIRONMENT (for testing)
  TAILGATE_READ_ONLY=1       Same as --read-only
  TAILGATE_TAILSCALE_BIN     tailscale binary to use
  TAILGATE_PKEXEC_BIN        pkexec binary to use (Linux)
  TAILGATE_OPEN_BIN          Program that opens links (default xdg-open, open, or rundll32 on Windows)
  TAILGATE_CONFIG_DIR        Directory for state.json
  TAILGATE_IPCHECK_URL       URL for Check my IP
  TAILGATE_SHARES_TOOLS      Folder of stand-ins for gio, smbclient, net, open...
";

fn main() {
    #[cfg(windows)]
    if std::env::args().len() > 1 {
        use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
        unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    }
    let mut read_only = std::env::var("TAILGATE_READ_ONLY").is_ok_and(|v| !v.is_empty() && v != "0");
    let mut minimized = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--read-only" => read_only = true,
            "--login" => minimized = true,
            "--shares" => std::process::exit(shares_cli::list()),
            "--connect-share" | "--disconnect-share" => {
                let Some(target) = args.next() else {
                    eprintln!("tailgate: {arg} needs a share, like //host/Share.");
                    std::process::exit(2);
                };
                std::process::exit(shares_cli::connect(&target, arg == "--connect-share", read_only));
            }
            "--version" | "-V" => {
                println!("tailgate {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--where" => {
                let client = tailgate::cli::Client::from_env(true);
                println!("{} ({:?})", client.bin.display(), client.variant);
                if let Some(note) = client.variant.note() {
                    println!("{note}");
                }
                let answer = client.read(tailgate::cli::Query::Status).and_then(|text| {
                    tailgate::status::parse_status(&text).map_err(tailgate::cli::CliError::Malformed)
                });
                match answer {
                    Ok(status) => println!("Answers: {:?}, {} exit nodes", status.backend, status.exit_nodes().count()),
                    Err(e) => {
                        println!("No answer: {e}");
                        std::process::exit(1);
                    }
                }
                return;
            }
            "--help" | "-h" => {
                print!("{HELP}");
                return;
            }
            other => {
                eprintln!("tailgate: unknown argument '{other}'. Try --help.");
                std::process::exit(2);
            }
        }
    }
    if let Err(e) = app::run(read_only, minimized) {
        eprintln!("tailgate: couldn't open a window: {e}");
        std::process::exit(1);
    }
}

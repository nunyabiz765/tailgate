// Windows: a GUI app, so double-clicking doesn't open a console window too.
// --version and --help attach to the console they were started from.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;

slint::include_modules!();

const HELP: &str = "\
Tailgate: manage Tailscale and its Mullvad exit nodes

USAGE
  tailgate [--read-only]

FLAGS
  --read-only   Show live status, but refuse every change before it runs
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
";

fn main() {
    #[cfg(windows)]
    if std::env::args().len() > 1 {
        use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
        unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    }
    let mut read_only = std::env::var("TAILGATE_READ_ONLY").is_ok_and(|v| !v.is_empty() && v != "0");
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--read-only" => read_only = true,
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
    if let Err(e) = app::run(read_only) {
        eprintln!("tailgate: couldn't open a window: {e}");
        std::process::exit(1);
    }
}

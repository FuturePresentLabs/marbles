//! `mb`: the Marbles CLI over `soil:records`.
//!
//! The host decides which records book and collection this runs against; this
//! binary names only the `marbles` collection and the record schemas, exactly as
//! `docs/peer-sync.md` requires of a Spore. Everything that selects an endpoint,
//! a token or a filesystem path belongs to the host that launched it.
//!
//! The command surface itself lives in `records_cli` (grammar) and
//! `records_exec` (translation to `Store` calls) so it is testable on the host;
//! this file is the wasm entry point and the one place that opens the book.

use std::collections::BTreeMap;
use std::io::Write;
use std::process::ExitCode;

use marbles::adapter::Adapter;
use marbles::records_cli;
use marbles::records_exec;
use marbles::store;

fn main() -> ExitCode {
    // The release wasm is built with `panic = "abort"`, and a trap carries no
    // cause of its own. Write the reason and flush it by hand before the abort,
    // so a failure inside the guest is readable at the shell instead of being a
    // bare wasm backtrace.
    std::panic::set_hook(Box::new(|info| {
        use std::io::Write;
        let mut stderr = std::io::stderr();
        let _ = writeln!(stderr, "mb: {info}");
        let _ = stderr.flush();
    }));
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("mb: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let env = MARBLES_ENV
        .iter()
        .filter_map(|key| std::env::var(key).ok().map(|value| (*key, value)))
        .map(|(key, value)| (key.to_string(), value))
        .collect::<BTreeMap<_, _>>();
    let invocation = records_cli::parse(&argv, &env)?;
    let store = Adapter::open("marbles".into()).map_err(|error| error.to_string())?;
    let now = store::now();
    let answer = records_exec::execute(&store, &invocation, now)?;
    print_json(&answer)
}

/// The only environment a caller can use to scope an invocation. A Spore has no
/// ambient environment; these three are the documented `mb` contract, and the
/// host is what puts them there.
const MARBLES_ENV: &[&str] = &["MARBLES_PROJECT", "MARBLES_PREFIX", "MARBLES_ACTOR"];

fn print_json(value: &serde_json::Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    // `println!` panics when the write fails, and a host caps a command's stdout
    // with a pipe: `mb list --all` over a fleet-sized store hits that cap, and a
    // panic turns a size limit into a wasm trap with a backtrace for an error
    // message. Report the cap and what narrows the answer instead.
    let mut stdout = std::io::stdout();
    stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.write_all(b"\n"))
        .and_then(|()| stdout.flush())
        .map_err(|error| {
            format!(
                "could not write the answer ({error}); the host caps a command's stdout, so \
                 narrow the query with --project or --limit"
            )
        })
}

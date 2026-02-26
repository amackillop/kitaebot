mod agent;
mod error;
mod heartbeat;
mod lock;
mod provider;
mod repl;
mod session;
mod tools;
mod types;
mod workspace;

use heartbeat::Outcome;
#[cfg(not(feature = "mock-network"))]
use provider::OpenRouterProvider;
use provider::Provider;
#[cfg(feature = "mock-network")]
use provider::StubProvider;
use std::io::{self, Write};
use tools::{Exec, Tool, Tools};
use workspace::Workspace;

#[tokio::main]
async fn main() {
    #[cfg(feature = "mock-network")]
    let provider = StubProvider;

    #[cfg(not(feature = "mock-network"))]
    let provider = OpenRouterProvider::from_env().unwrap_or_else(|e| {
        eprintln!("Failed to initialize provider: {e}");
        eprintln!("Set OPENROUTER_API_KEY environment variable");
        std::process::exit(1);
    });

    let workspace = Workspace::init().unwrap_or_else(|e| {
        eprintln!("Failed to initialize workspace: {e}");
        std::process::exit(1);
    });

    let tools = Tools::new(vec![Tool::Exec(Exec::new(workspace.path()))]);

    match std::env::args().nth(1).as_deref() {
        Some("heartbeat") => run_heartbeat(&workspace, &provider, &tools).await,
        Some(cmd) => {
            eprintln!("Unknown command: {cmd}");
            std::process::exit(1);
        }
        None => run_repl(&workspace, &provider, &tools).await,
    }
}

async fn run_heartbeat<P: Provider>(workspace: &Workspace, provider: &P, tools: &Tools) {
    match heartbeat::run(workspace, provider, tools).await {
        Ok(Outcome::Executed(response)) => {
            eprintln!("Heartbeat complete: {response}");
        }
        Ok(Outcome::Skipped(reason)) => {
            eprintln!("Heartbeat skipped: {reason}");
        }
        Err(e) => {
            eprintln!("Heartbeat failed: {e}");
            std::process::exit(1);
        }
    }
}

async fn run_repl<P: Provider>(workspace: &Workspace, provider: &P, tools: &Tools) {
    let Ok(_lock) = Lock::acquire(&workspace.repl_lock_path()) else {
        eprintln!("Another session is already running");
        std::process::exit(1);
    };

    let mut session = Session::load(&workspace.session_path()).unwrap_or_else(|e| {
        eprintln!("Failed to load session: {e}");
        std::process::exit(1);
    });

    let mut system_prompt = workspace.system_prompt();

    let n = session.messages().len();
    if n == 0 {
        println!("New session\n");
    } else {
        println!("Resumed session ({n} messages)\n");
    }

    loop {
        print!("> ");
        io::stdout().flush().unwrap();

        let mut input = String::new();
        match io::stdin().read_line(&mut input) {
            Ok(0) | Err(_) => break, // EOF or read error
            Ok(_) => {}
        }

        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        if input == "exit" {
            break;
        }
        if input == "/new" {
            session.clear();
            if let Err(e) = session.save(&workspace.session_path()) {
                eprintln!("Failed to save session: {e}");
            }
            system_prompt = workspace.system_prompt();
            println!("Session cleared.\n");
            continue;
        }

        match run_turn(&mut session, &system_prompt, input, provider, tools).await {
            Ok(response) => {
                println!("{response}\n");
                if let Err(e) = session.save(&workspace.session_path()) {
                    eprintln!("Failed to save session: {e}");
                }
            }
            Err(e) => eprintln!("Error: {e}\n"),
        }
    }
}

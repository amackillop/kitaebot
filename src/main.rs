mod agent;
mod error;
mod provider;
mod session;
mod tools;
mod types;
mod workspace;

use agent::run_turn;
#[cfg(not(feature = "mock-network"))]
use provider::OpenRouterProvider;
#[cfg(feature = "mock-network")]
use provider::StubProvider;
use session::Session;
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

        match run_turn(&mut session, &system_prompt, input, &provider, &tools).await {
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

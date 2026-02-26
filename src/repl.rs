//! Interactive REPL for direct conversation.
//!
//! Provides a simple stdin/stdout loop for chatting with the agent.
//! Acquires a session lock to prevent concurrent access.

use crate::agent::run_turn;
use crate::lock::Lock;
use crate::provider::Provider;
use crate::session::Session;
use crate::tools::Tools;
use crate::workspace::Workspace;
use std::io::{self, Write};

/// Run the interactive REPL loop.
///
/// Acquires the REPL lock, loads the session, and enters a read-eval-print
/// loop until the user sends EOF or types `exit`.
pub async fn run<P: Provider>(workspace: &Workspace, provider: &P, tools: &Tools) {
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
            Ok(0) | Err(_) => break,
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

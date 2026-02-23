mod agent;
mod error;
mod provider;
mod tools;
mod types;

use agent::run_turn;
#[cfg(not(feature = "mock-network"))]
use provider::OpenRouterProvider;
#[cfg(feature = "mock-network")]
use provider::StubProvider;
use std::io::{self, Write};
use tools::{ExecTool, ToolRegistry};

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

    let mut tools = ToolRegistry::new();
    tools.register(Box::new(ExecTool::new(".")));

    println!("Kitaebot REPL");
    println!("Type 'exit' to quit\n");

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

        match run_turn(input, &provider, &tools).await {
            Ok(response) => println!("{response}\n"),
            Err(e) => eprintln!("Error: {e}\n"),
        }
    }
}

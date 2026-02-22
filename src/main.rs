mod agent;
mod error;
mod provider;
mod tools;
mod types;

use agent::run_turn;
use provider::StubProvider;
use std::io::{self, Write};
use tools::{StubTool, ToolRegistry};

#[tokio::main]
async fn main() {
    let provider = StubProvider;
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(StubTool));

    println!("Kitaebot REPL (stub mode)");
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

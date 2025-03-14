use clap::Parser;
use std::fmt::Display;
use std::io::{self, BufRead};
use tracing::error;

use common::{extract_key, form_key, init_tracing, set_default_rust_log, Session};
use rpc::gateway::{CommandResult, Status};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[arg(
        long,
        short,
        default_value = "0.0.0.0:24000",
        help = "The address to connect to."
    )]
    connect_addr: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging
    set_default_rust_log("info");
    init_tracing();

    // Parse command line arguments
    let cli = Cli::parse();

    // Connect to the server
    let address = format!("http://{}", cli.connect_addr);
    let mut session = Session::remote("stdI/O", address).await?;

    // Process commands from stdin
    let stdin = io::stdin();
    let reader = stdin.lock();

    for line in reader.lines() {
        let line = line?;
        let tokens = tokenize(&line);
        if tokens.is_empty() || tokens[0].is_empty() {
            continue;
        }

        match tokens[0].to_uppercase().as_str() {
            "PUT" | "SCAN" | "SWAP" => {
                if tokens.len() != 3 {
                    error!("PUT/SCAN/SWAP requires 2 arguments: <key> <value>");
                    continue;
                }
                let output =
                    execute_command(&mut session, &tokens[0].to_uppercase(), &tokens[1..]).await;
                println!("{}", output);
            }
            "GET" | "DELETE" => {
                if tokens.len() != 2 {
                    error!("{} requires 1 argument: <key>", tokens[0].to_uppercase());
                    continue;
                }
                let output =
                    execute_command(&mut session, &tokens[0].to_uppercase(), &tokens[1..]).await;
                println!("{}", output);
            }
            "STOP" => {
                println!("STOP");
                break;
            }
            _ => {
                error!("Unknown command: {}", line);
            }
        }
    }

    Ok(())
}

/// Split a line into tokens by whitespace
fn tokenize(line: &str) -> Vec<String> {
    line.split_whitespace().map(String::from).collect()
}

/// Execute a command with retry logic
async fn execute_command(session: &mut Session, cmd: &str, args: &[String]) -> String {
    const MAX_RETRIES: u32 = 10;
    let mut retries = 0;

    loop {
        // Create a new command and execute it
        session.new_command().unwrap();
        session.add_operation(cmd, args).unwrap();

        match handle_result(session.finish_command().await) {
            Ok(output) if output == "Aborted" && retries < MAX_RETRIES => {
                retries += 1;
                // Silently retry aborted commands
                continue;
            }
            Ok(output) if output == "Aborted" => {
                panic!("Command still aborted after {} retries", MAX_RETRIES);
            }
            Ok(output) => return output,
            Err(e) => panic!("{}", e),
        }
    }
}

/// Process command results and format the output
fn handle_result(result: anyhow::Result<Vec<CommandResult>>) -> Result<String, String> {
    match result {
        Ok(cmd_results) => {
            // Ensure we have at least one result
            if cmd_results.is_empty() {
                return Err("No command results returned".to_string());
            }

            // Handle single-command results (GET, PUT, SWAP, DELETE)
            if cmd_results.len() == 1 {
                handle_single_command_result(&cmd_results[0])
            }
            // Handle multi-command results (SCAN across partitions)
            else {
                handle_scan_results(&cmd_results)
            }
        }
        Err(e) => Err(error(e)),
    }
}

/// Process single-command results (GET, PUT, DELETE, SWAP)
fn handle_single_command_result(cmd_result: &CommandResult) -> Result<String, String> {
    // Check for command errors
    if cmd_result.has_err {
        return Err(if !cmd_result.content.is_empty() {
            cmd_result.content.clone()
        } else {
            "Command failed".to_string()
        });
    }

    // Ensure we have exactly one operation result
    if cmd_result.ops.len() != 1 {
        return Err(format!(
            "Expected 1 operation result, got {}",
            cmd_result.ops.len()
        ));
    }

    // Return result based on command status
    match cmd_result.status() {
        Status::Aborted => Ok("Aborted".to_string()),
        Status::Committed => Ok(cmd_result.ops[0].content.to_owned()),
    }
}

/// Process multi-command results from SCAN operations
fn handle_scan_results(cmd_results: &[CommandResult]) -> Result<String, String> {
    // Check for errors in any of the results
    let has_err = cmd_results.iter().any(|res| res.has_err);
    if has_err {
        let error_content: String = cmd_results
            .iter()
            .filter_map(|res| {
                if res.has_err {
                    Some(res.content.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        return Err(if !error_content.is_empty() {
            error_content
        } else {
            "Command failed".to_string()
        });
    }

    // Process SCAN operation results
    let mut min_start_key = None;
    let mut max_end_key = None;
    let mut scan_entries = Vec::new();
    let table_name = "usertable_user".to_string(); // Hardcoded for YCSB

    // Process all results to find boundaries and collect entries
    for result in cmd_results {
        if result.ops.len() != 1 {
            continue;
        }

        let op = &result.ops[0];
        if op.has_err {
            continue;
        }

        let lines: Vec<&str> = op.content.lines().collect();
        if lines.is_empty() {
            continue;
        }

        // Parse the SCAN line to get keys
        if lines[0].trim().starts_with("SCAN ") {
            let parts: Vec<&str> = lines[0].split_whitespace().collect();
            if parts.len() >= 3 {
                if let Ok((_, start_key)) = extract_key(parts[1]) {
                    if min_start_key.is_none() || start_key < min_start_key.unwrap() {
                        min_start_key = Some(start_key);
                    }
                }

                if let Ok((_, end_key)) = extract_key(parts[2]) {
                    if max_end_key.is_none() || end_key > max_end_key.unwrap() {
                        max_end_key = Some(end_key);
                    }
                }
            }
        }

        // Collect entries (skipping the SCAN header and footer)
        for line in lines.iter().skip(1) {
            let trimmed = line.trim();
            if !trimmed.is_empty() && !trimmed.contains("SCAN END") {
                scan_entries.push(trimmed.to_string());
            }
        }
    }

    // If no valid keys were found, return empty result
    if min_start_key.is_none() || max_end_key.is_none() {
        return Ok("SCAN BEGIN\nSCAN END".to_string());
    }

    // Format the combined scan result
    let mut scan_result = Vec::new();
    scan_result.push(format!(
        "SCAN {} {} BEGIN",
        form_key(&table_name, min_start_key.unwrap()),
        form_key(&table_name, max_end_key.unwrap())
    ));

    // Sort entries by key
    scan_entries.sort_by(|a, b| {
        let a_parts: Vec<&str> = a.split_whitespace().collect();
        let b_parts: Vec<&str> = b.split_whitespace().collect();

        if a_parts.is_empty() || b_parts.is_empty() {
            return std::cmp::Ordering::Equal;
        }

        let a_key = a_parts[0];
        let b_key = b_parts[0];

        if let (Ok((_, a_num)), Ok((_, b_num))) = (extract_key(a_key), extract_key(b_key)) {
            a_num.cmp(&b_num)
        } else {
            a_key.cmp(b_key) // Fallback to string comparison
        }
    });

    // Add sorted entries to result
    for entry in scan_entries {
        scan_result.push(entry);
    }

    scan_result.push("SCAN END".to_string());

    // Determine overall status
    let any_aborted = cmd_results
        .iter()
        .any(|res| res.status == Status::Aborted.into());

    if any_aborted {
        Ok("Aborted".to_string())
    } else {
        Ok(scan_result.join("\n"))
    }
}

/// Format an error message
fn error(msg: impl Display) -> String {
    format!("ERROR {msg}")
}

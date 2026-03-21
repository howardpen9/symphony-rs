use std::env;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;

use symphony_rs::config::ServiceConfig;
use symphony_rs::linear::LinearTrackerClient;
use symphony_rs::service::SymphonyService;
use symphony_rs::workflow::WorkflowLoader;

const ACK_FLAG: &str = "--i-understand-that-this-will-be-running-without-the-usual-guardrails";

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("validate");
    let workflow_path =
        parse_workflow_path(&args[1..]).unwrap_or_else(|| PathBuf::from("WORKFLOW.md"));
    let tracker = Arc::new(LinearTrackerClient::new());

    match command {
        "validate" => validate(workflow_path),
        "once" => {
            let mut service = SymphonyService::from_workflow_path(workflow_path, tracker)?;
            service.run_single_cycle();
            println!("{}", service.snapshot_json()?);
            Ok(())
        }
        "snapshot" => {
            let service = SymphonyService::from_workflow_path(workflow_path, tracker)?;
            println!("{}", service.snapshot_json()?);
            Ok(())
        }
        "serve" => {
            if !args.iter().any(|value| value == ACK_FLAG) {
                return Err(format!("missing required acknowledgement flag `{ACK_FLAG}`").into());
            }
            let mut service = SymphonyService::from_workflow_path(workflow_path, tracker)?;
            service.run_forever();
        }
        _ => Err(format!(
            "unknown command `{command}`\nusage: symphony-rs [validate|once|snapshot|serve] [WORKFLOW.md] [{ACK_FLAG}]"
        )
        .into()),
    }
}

fn validate(workflow_path: PathBuf) -> Result<(), Box<dyn Error>> {
    let workflow = WorkflowLoader::from_path(&workflow_path)?;
    let config = ServiceConfig::from_workflow_definition(&workflow)?;
    let validation_errors = config.validate_for_dispatch();

    println!("workflow_path={}", workflow_path.display());
    println!(
        "prompt_present={}",
        !workflow.prompt_template.trim().is_empty()
    );
    println!("dispatch_ready={}", validation_errors.is_empty());

    if !validation_errors.is_empty() {
        println!("validation_errors:");
        for error in validation_errors {
            println!("  - {error}");
        }
    }

    Ok(())
}

fn parse_workflow_path(args: &[String]) -> Option<PathBuf> {
    args.iter()
        .find(|value| !value.starts_with("--"))
        .map(PathBuf::from)
}

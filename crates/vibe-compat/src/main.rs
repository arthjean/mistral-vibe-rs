#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use vibe_app_server::transport::benchmark_fake_provider_chunk_latency;
use vibe_compat::baseline;
use vibe_compat::differential::{
    build_report, compare_directories, report_is_release_ready, write_reports,
};
use vibe_compat::matrix;
use vibe_compat::model::SupportClass;
use vibe_compat::oracle::{record_all, validate_corpus, validate_scenarios};
use vibe_compat::rust_recorder::record_rust_all;
use vibe_compat::workspace;

#[derive(Parser)]
#[command(
    name = "vibe-compat",
    version,
    about = "Mistral Vibe compatibility harness"
)]
struct Arguments {
    #[arg(long, default_value = ".")]
    root: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(hide = true)]
    McpFixture,
    Provision {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        sync: bool,
    },
    Validate {
        #[arg(long)]
        upstream: Option<PathBuf>,
        #[arg(long)]
        corpus: Option<PathBuf>,
    },
    Record {
        #[arg(long)]
        upstream: Option<PathBuf>,
        #[arg(long, default_value = "compat/corpus/upstream-2.23.1")]
        output: PathBuf,
    },
    RecordRust {
        #[arg(long, default_value = "compat/corpus/rust-release-1")]
        output: PathBuf,
        #[arg(long, default_value_t = 1)]
        release: u32,
    },
    Compare {
        #[arg(long)]
        expected: PathBuf,
        #[arg(long)]
        actual: PathBuf,
        #[arg(long)]
        report_json: PathBuf,
        #[arg(long)]
        report_markdown: PathBuf,
        #[arg(long, default_value_t = 0)]
        release: u32,
    },
    BenchmarkStreaming {
        #[arg(long, default_value_t = 100_000)]
        chunks: usize,
        #[arg(
            long,
            default_value = "compat/reports/release-1-streaming-latency.json"
        )]
        output: PathBuf,
    },
    SchemaDigest,
}

fn main() -> Result<(), Box<dyn Error>> {
    let arguments = Arguments::parse();
    let root = arguments.root;
    match arguments.command {
        Command::McpFixture => vibe_compat::rust_recorder::serve_mcp_stdio_fixture()?,
        Command::Provision { source, sync } => {
            let checkout = baseline::provision(&root, &source, sync)?;
            println!("{}", checkout.display());
        }
        Command::Validate { upstream, corpus } => {
            workspace::validate(&root)?;
            let baseline = baseline::load()?;
            let checkout = upstream.unwrap_or_else(|| baseline::checkout_path(&root, &baseline));
            baseline::validate_checkout(&checkout, &baseline)?;
            let matrix = matrix::validate(&checkout)?;
            let rows = matrix.rows.iter().map(|row| row.id.as_str()).collect();
            let scenarios = validate_scenarios(&rows)?;
            if let Some(corpus) = corpus {
                validate_corpus(
                    &root.join(corpus),
                    &scenarios.scenarios,
                    &baseline.version,
                    baseline.fixture_schema_version,
                )?;
            }
            let expected_digest = fs::read_to_string(root.join("compat/protocol-schema.sha256"))?;
            let actual_digest = vibe_protocol::protocol_schema_digest();
            if expected_digest.trim() != actual_digest {
                return Err(format!(
                    "protocol schema digest changed: expected {}, got {actual_digest}",
                    expected_digest.trim()
                )
                .into());
            }
            println!(
                "{{\"baseline\":\"{}\",\"matrixRows\":{},\"scenarios\":{},\"schemaDigest\":\"{}\",\"status\":\"pass\"}}",
                baseline.version,
                matrix.rows.len(),
                scenarios.scenarios.len(),
                actual_digest
            );
        }
        Command::Record { upstream, output } => {
            let baseline = baseline::load()?;
            let checkout = upstream.unwrap_or_else(|| baseline::checkout_path(&root, &baseline));
            baseline::validate_checkout(&checkout, &baseline)?;
            let matrix = matrix::validate(&checkout)?;
            let rows = matrix.rows.iter().map(|row| row.id.as_str()).collect();
            let scenarios = validate_scenarios(&rows)?;
            let paths = record_all(
                &root,
                &checkout,
                &root.join(output),
                &baseline.version,
                &baseline.python_version,
                baseline.fixture_schema_version,
                &scenarios.scenarios,
            )?;
            println!("recorded {} fixtures", paths.len());
        }
        Command::RecordRust { output, release } => {
            let baseline = baseline::load()?;
            let checkout = baseline::checkout_path(&root, &baseline);
            let matrix = matrix::validate(&checkout)?;
            let rows = matrix.rows.iter().map(|row| row.id.as_str()).collect();
            let scenarios = validate_scenarios(&rows)?;
            let required_rows = matrix
                .rows
                .iter()
                .filter(|row| row.required_release <= release)
                .map(|row| row.id.as_str())
                .collect::<BTreeSet<_>>();
            let selected = scenarios
                .scenarios
                .iter()
                .filter(|scenario| required_rows.contains(scenario.matrix_row.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            let paths = record_rust_all(
                &root,
                &root.join(output),
                &baseline.version,
                baseline.fixture_schema_version,
                &selected,
            )?;
            println!("recorded {} Rust fixtures", paths.len());
        }
        Command::Compare {
            expected,
            actual,
            report_json,
            report_markdown,
            release,
        } => {
            let baseline = baseline::load()?;
            let checkout = baseline::checkout_path(&root, &baseline);
            let matrix = matrix::validate(&checkout)?;
            let rows = matrix.rows.iter().map(|row| row.id.as_str()).collect();
            let scenarios = validate_scenarios(&rows)?;
            let required_rows = matrix
                .rows
                .iter()
                .filter(|row| row.required_release <= release)
                .map(|row| row.id.as_str())
                .collect::<BTreeSet<_>>();
            let selected = scenarios
                .scenarios
                .iter()
                .filter(|scenario| required_rows.contains(scenario.matrix_row.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            let intentional = matrix
                .rows
                .iter()
                .filter(|row| row.divergence_status == "intentional")
                .map(|row| row.id.as_str())
                .collect::<BTreeSet<_>>();
            let blocked = matrix
                .rows
                .iter()
                .filter(|row| row.rust_status == "blocked")
                .map(|row| row.id.as_str())
                .collect::<BTreeSet<_>>();
            let excluded = matrix
                .rows
                .iter()
                .filter(|row| row.support == SupportClass::Excluded)
                .map(|row| row.id.as_str())
                .collect::<BTreeSet<_>>();
            let verdicts = compare_directories(
                &root.join(expected),
                &root.join(actual),
                &selected,
                &intentional,
                &blocked,
                &excluded,
            )?;
            let report = build_report(&root, &matrix, release, verdicts);
            write_reports(
                &report,
                &root.join(report_json),
                &root.join(report_markdown),
            )?;
            if !report_is_release_ready(&report) {
                return Err("compatibility report is not release-ready".into());
            }
        }
        Command::BenchmarkStreaming { chunks, output } => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let report = runtime.block_on(benchmark_fake_provider_chunk_latency(chunks))?;
            let output = root.join(output);
            if let Some(parent) = output.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut encoded = serde_json::to_vec_pretty(&report)?;
            encoded.push(b'\n');
            fs::write(&output, encoded)?;
            println!("{}", serde_json::to_string(&report)?);
            if !report.release_gate_passed {
                return Err("streaming latency release gate failed".into());
            }
        }
        Command::SchemaDigest => println!("{}", vibe_protocol::protocol_schema_digest()),
    }
    Ok(())
}

//! Benchmark command orchestration.

use std::{fs, path::Path, path::PathBuf};

use bpm::bench::{self, BenchmarkResult, CompareOptions, RunSuiteOptions, ScenarioKind, Tool};

pub(super) struct Options {
    pub fixture: String,
    pub scenario: Option<String>,
    pub tools: String,
    pub require_tools: bool,
    pub runs: usize,
    pub json: Option<PathBuf>,
    pub save_baseline: Option<PathBuf>,
    pub compare_baseline: Option<PathBuf>,
    pub baseline_informational: bool,
    pub regression_envelope: f64,
    pub profile_bpm: Option<PathBuf>,
    pub list: bool,
}

pub(super) fn run(options: Options) -> anyhow::Result<()> {
    if options.list {
        println!("Available scenarios:");
        for scenario in ScenarioKind::all() {
            println!("  {:<18} {}", scenario.name(), scenario.describe());
        }
        println!();
        println!("Available fixtures:");
        for fixture in bench::FIXTURES {
            println!(
                "  {:<18} packages: {}",
                fixture.name,
                fixture.packages.join(" ")
            );
        }
        return Ok(());
    }

    if options.runs < 1 {
        anyhow::bail!("--runs must be at least 1");
    }
    if !(options.regression_envelope.is_finite() && options.regression_envelope > 0.0) {
        anyhow::bail!("--regression-envelope must be a positive finite number");
    }
    if options.profile_bpm.is_some() && !Tool::Bpm.detect() {
        anyhow::bail!("--profile-bpm requires bpm to be available");
    }

    let fixture = bench::FIXTURES
        .iter()
        .find(|fixture| fixture.name == options.fixture)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unknown fixture '{}'; use --list to see available",
                options.fixture
            )
        })?;
    let scenarios = parse_scenarios(options.scenario.as_deref())?;
    let tools = parse_tools(&options.tools)?;

    eprintln!(
        "benchmark: fixture={}, scenarios={}, requested_tools={}, runs={}, require_tools={}",
        options.fixture,
        scenarios.len(),
        tools.len(),
        options.runs,
        options.require_tools
    );
    eprintln!();

    let suite = bench::run_suite(
        &scenarios,
        fixture,
        &tools,
        &RunSuiteOptions {
            num_runs: options.runs,
            require_tools: options.require_tools,
        },
    )?;

    if options.save_baseline.is_none() && options.json.is_none() {
        suite.print_text();
    }
    if let Some(dir) = options.save_baseline {
        fs::create_dir_all(&dir)
            .map_err(|e| anyhow::anyhow!("failed to create baseline dir {}: {e}", dir.display()))?;
        let machine = suite
            .results
            .first()
            .map(|result| result.system.machine.clone())
            .unwrap_or_else(|| bench::SystemInfo::capture().machine);
        let path = dir.join(format!("{}-{}.json", slugify(&machine), current_ymd()));
        write_suite_json(&path, &suite.to_json())?;
        println!("baseline written to {}", path.display());
    }
    if let Some(path) = options.json {
        write_suite_json(&path, &suite.to_json())?;
        eprintln!("results written to {}", path.display());
    }
    if let Some(dir) = options.profile_bpm {
        let reference = suite.results.first().ok_or_else(|| {
            anyhow::anyhow!("cannot write BPM profiles for an empty benchmark suite")
        })?;
        bench::profile_bpm_scenarios(
            &scenarios,
            fixture,
            &dir,
            &reference.system,
            &reference.versions,
        )?;
        eprintln!(
            "bpm profile manifest written to {}",
            dir.join("manifest.json").display()
        );
    }
    if let Some(path) = options.compare_baseline {
        let baseline = read_benchmark_results(&path)?;
        let rows = bench::compare_results_against_baseline(
            &baseline,
            &suite.results,
            &CompareOptions {
                regression_envelope: options.regression_envelope,
                informational: options.baseline_informational,
            },
        )?;
        for row in rows {
            println!(
                "baseline fixture={} scenario={} tool={} baseline={:.3}ms current={:.3}ms ratio={:.3} baseline_machine={} current_machine={} baseline_versions={:?} current_versions={:?}",
                row.fixture,
                row.scenario,
                row.tool,
                row.baseline_median_ms,
                row.current_median_ms,
                row.ratio,
                row.baseline_machine,
                row.current_machine,
                row.baseline_versions,
                row.current_versions,
            );
        }
    }

    Ok(())
}

fn parse_scenarios(name: Option<&str>) -> anyhow::Result<Vec<ScenarioKind>> {
    if let Some(name) = name {
        let scenario = ScenarioKind::all()
            .into_iter()
            .find(|scenario| scenario.name() == name)
            .ok_or_else(|| {
                anyhow::anyhow!("unknown scenario '{}'; use --list to see available", name)
            })?;
        Ok(vec![scenario])
    } else {
        Ok(ScenarioKind::all())
    }
}

fn parse_tools(raw: &str) -> anyhow::Result<Vec<Tool>> {
    raw.split(',')
        .map(|value| match value.trim() {
            "npm" => Ok(Tool::Npm),
            "pnpm" => Ok(Tool::Pnpm),
            "bpm" => Ok(Tool::Bpm),
            "yarn" => Ok(Tool::Yarn),
            "bun" => Ok(Tool::Bun),
            other => Err(anyhow::anyhow!("unknown tool '{other}'")),
        })
        .collect()
}

fn read_benchmark_results(path: &Path) -> anyhow::Result<Vec<BenchmarkResult>> {
    let bytes = fs::read(path)
        .map_err(|e| anyhow::anyhow!("failed to read baseline {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| anyhow::anyhow!("failed to parse baseline {}: {e}", path.display()))
}

fn write_suite_json(path: &Path, json: &str) -> anyhow::Result<()> {
    fs::write(path, json)
        .map_err(|e| anyhow::anyhow!("failed to write JSON to {}: {e}", path.display()))
}

fn current_ymd() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let days = seconds / 86_400;
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}{month:02}{day:02}")
}

fn slugify(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

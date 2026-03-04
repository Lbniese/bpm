//! Benchmark command orchestration.

use std::{fs, path::PathBuf};

use bpm::bench::{self, ScenarioKind, Tool};

pub(super) struct Options {
    pub fixture: String,
    pub scenario: Option<String>,
    pub tools: String,
    pub runs: usize,
    pub json: Option<PathBuf>,
    pub save_baseline: Option<PathBuf>,
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

    let fixture = bench::FIXTURES
        .iter()
        .find(|fixture| fixture.name == options.fixture)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unknown fixture '{}'; use --list to see available",
                options.fixture
            )
        })?;
    let scenarios = if let Some(name) = options.scenario {
        let scenario = ScenarioKind::all()
            .into_iter()
            .find(|scenario| scenario.name() == name)
            .ok_or_else(|| {
                anyhow::anyhow!("unknown scenario '{}'; use --list to see available", name)
            })?;
        vec![scenario]
    } else {
        ScenarioKind::all()
    };
    let tools: Vec<Tool> = options
        .tools
        .split(',')
        .map(|value| match value.trim() {
            "npm" => Ok(Tool::Npm),
            "pnpm" => Ok(Tool::Pnpm),
            "bpm" => Ok(Tool::Bpm),
            "yarn" => Ok(Tool::Yarn),
            "bun" => Ok(Tool::Bun),
            other => Err(anyhow::anyhow!("unknown tool '{other}'")),
        })
        .collect::<anyhow::Result<Vec<_>>>()?
        .into_iter()
        .filter(|tool| {
            if tool.detect() {
                true
            } else {
                eprintln!("warning: {} not found on $PATH, skipping", tool.name());
                false
            }
        })
        .collect();
    if tools.is_empty() {
        anyhow::bail!("no tools available (tried: {})", options.tools);
    }
    if options.runs < 1 {
        anyhow::bail!("--runs must be at least 1");
    }
    eprintln!(
        "benchmark: fixture={}, scenarios={}, tools={}, runs={}",
        options.fixture,
        scenarios.len(),
        tools.len(),
        options.runs
    );
    eprintln!();
    let suite = bench::run_suite(&scenarios, fixture, &tools, options.runs)?;
    if let Some(dir) = options.save_baseline {
        fs::create_dir_all(&dir)
            .map_err(|e| anyhow::anyhow!("failed to create baseline dir {}: {e}", dir.display()))?;
        let machine = bench::SystemInfo::capture().machine;
        let path = dir.join(format!("{}-{}.json", slugify(&machine), current_ymd()));
        fs::write(&path, suite.to_json())
            .map_err(|e| anyhow::anyhow!("failed to write baseline {}: {e}", path.display()))?;
        println!("baseline written to {}", path.display());
    } else if let Some(path) = options.json {
        fs::write(&path, suite.to_json())
            .map_err(|e| anyhow::anyhow!("failed to write JSON to {}: {e}", path.display()))?;
        eprintln!("results written to {}", path.display());
    } else {
        suite.print_text();
    }
    Ok(())
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

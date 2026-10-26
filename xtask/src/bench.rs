use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub(crate) struct Targets {
    /// Multiplier applied to every limit on shared CI runners (ADR-0014).
    ci_factor: f64,
    #[serde(default, rename = "bench")]
    benches: Vec<Target>,
}

#[derive(Debug, Deserialize)]
struct Target {
    /// Criterion benchmark id, i.e. the path below `target/criterion`.
    id: String,
    statistic: Statistic,
    max_ns: f64,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Statistic {
    Mean,
    Median,
}

impl Statistic {
    fn key(self) -> &'static str {
        match self {
            Statistic::Mean => "mean",
            Statistic::Median => "median",
        }
    }
}

#[derive(Debug, PartialEq)]
pub(crate) struct Outcome {
    id: String,
    measured_ns: f64,
    limit_ns: f64,
}

impl Outcome {
    fn passed(&self) -> bool {
        self.measured_ns <= self.limit_ns
    }
}

pub(crate) fn run(root: &Path, strict: bool) -> Result<(), String> {
    let path = root.join("benches").join("targets.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let targets: Targets =
        toml::from_str(&text).map_err(|e| format!("invalid {}: {e}", path.display()))?;
    if targets.benches.is_empty() {
        println!("bench-check: no benchmark targets defined yet");
        return Ok(());
    }

    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(Into::into)
        .unwrap_or_else(|| root.join("target"));
    let criterion_dir = target_dir.join("criterion");
    let factor = if strict { 1.0 } else { targets.ci_factor };

    let outcomes = evaluate(&targets, factor, |id, stat| {
        let file = criterion_dir.join(id).join("new").join("estimates.json");
        let text = std::fs::read_to_string(&file)
            .map_err(|e| format!("no estimate for {id} ({}): {e}", file.display()))?;
        point_estimate(&text, stat).map_err(|e| format!("{}: {e}", file.display()))
    })?;

    let mut failed = 0;
    for o in &outcomes {
        let verdict = if o.passed() { "ok" } else { "FAIL" };
        println!(
            "{verdict:4} {:<48} {:>12.1} ns  (limit {:.1} ns)",
            o.id, o.measured_ns, o.limit_ns
        );
        if !o.passed() {
            failed += 1;
        }
    }
    if failed > 0 {
        return Err(format!("{failed} benchmark(s) exceed their targets"));
    }
    Ok(())
}

fn evaluate(
    targets: &Targets,
    factor: f64,
    mut lookup: impl FnMut(&str, Statistic) -> Result<f64, String>,
) -> Result<Vec<Outcome>, String> {
    targets
        .benches
        .iter()
        .map(|t| {
            Ok(Outcome {
                id: t.id.clone(),
                measured_ns: lookup(&t.id, t.statistic)?,
                limit_ns: t.max_ns * factor,
            })
        })
        .collect()
}

fn point_estimate(estimates_json: &str, stat: Statistic) -> Result<f64, String> {
    let value: serde_json::Value =
        serde_json::from_str(estimates_json).map_err(|e| e.to_string())?;
    value[stat.key()]["point_estimate"]
        .as_f64()
        .ok_or_else(|| format!("missing {}.point_estimate", stat.key()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGETS: &str = r#"
        ci_factor = 2.0

        [[bench]]
        id = "codec/motion"
        statistic = "median"
        max_ns = 200

        [[bench]]
        id = "input/latency"
        statistic = "mean"
        max_ns = 500000
    "#;

    #[test]
    fn parses_targets_file() {
        let t: Targets = toml::from_str(TARGETS).unwrap();
        assert_eq!(t.benches.len(), 2);
        assert_eq!(t.benches[0].statistic, Statistic::Median);
    }

    #[test]
    fn empty_targets_file_is_valid() {
        let t: Targets = toml::from_str("ci_factor = 2.0").unwrap();
        assert!(t.benches.is_empty());
    }

    #[test]
    fn factor_relaxes_limits_and_failures_are_detected() {
        let t: Targets = toml::from_str(TARGETS).unwrap();
        let measured = |id: &str, _: Statistic| match id {
            "codec/motion" => Ok(350.0),
            _ => Ok(400_000.0),
        };
        let ci = evaluate(&t, 2.0, measured).unwrap();
        assert!(ci.iter().all(Outcome::passed));
        let strict = evaluate(&t, 1.0, measured).unwrap();
        assert!(!strict[0].passed());
        assert!(strict[1].passed());
    }

    #[test]
    fn missing_estimate_is_an_error() {
        let t: Targets = toml::from_str(TARGETS).unwrap();
        assert!(evaluate(&t, 1.0, |_, _| Err("missing".into())).is_err());
    }

    #[test]
    fn reads_criterion_estimates_format() {
        let json = r#"{"mean":{"confidence_interval":{"confidence_level":0.95,"lower_bound":1.0,"upper_bound":3.0},"point_estimate":2.5,"standard_error":0.1},
                      "median":{"confidence_interval":{"confidence_level":0.95,"lower_bound":1.0,"upper_bound":3.0},"point_estimate":2.0,"standard_error":0.1}}"#;
        assert_eq!(point_estimate(json, Statistic::Median).unwrap(), 2.0);
        assert_eq!(point_estimate(json, Statistic::Mean).unwrap(), 2.5);
        assert!(point_estimate("{}", Statistic::Mean).is_err());
    }
}

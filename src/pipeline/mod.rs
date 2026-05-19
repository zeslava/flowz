use crate::model::PipelineFile;
use std::collections::{HashMap, HashSet};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PipelineError {
    #[error("unsupported version {0}, expected 1")]
    UnsupportedVersion(u32),
    #[error("step '{0}' references unknown dependency '{1}'")]
    UnknownDependency(String, String),
    #[error("cycle detected involving step '{0}'")]
    CycleDetected(String),
    #[error("step '{step}': invalid run path '{path}'")]
    InvalidRunPath { step: String, path: String },
    #[error("step '{step}': invalid artifact path '{path}'")]
    InvalidArtifactPath { step: String, path: String },
    #[error("yaml parse error: {0}")]
    Parse(#[from] serde_yaml_ng::Error),
}

pub fn parse(yaml: &str) -> Result<PipelineFile, PipelineError> {
    let pf: PipelineFile = serde_yaml_ng::from_str(yaml)?;
    validate(&pf)?;
    Ok(pf)
}

pub fn validate(pf: &PipelineFile) -> Result<(), PipelineError> {
    if pf.version != 1 {
        return Err(PipelineError::UnsupportedVersion(pf.version));
    }

    let step_names: HashSet<&str> = pf.steps.keys().map(String::as_str).collect();

    for (name, step) in &pf.steps {
        // run must be a relative path, no absolute or parent traversal
        if step.run.starts_with('/') || step.run.contains("../") {
            return Err(PipelineError::InvalidRunPath {
                step: name.clone(),
                path: step.run.clone(),
            });
        }

        // artifact paths must be relative, no absolute or parent traversal
        for path in &step.artifacts {
            if path.starts_with('/') || path.contains("../") {
                return Err(PipelineError::InvalidArtifactPath {
                    step: name.clone(),
                    path: path.clone(),
                });
            }
        }

        for dep in &step.needs {
            if !step_names.contains(dep.as_str()) {
                return Err(PipelineError::UnknownDependency(name.clone(), dep.clone()));
            }
        }
    }

    detect_cycle(pf)?;

    Ok(())
}

fn detect_cycle(pf: &PipelineFile) -> Result<(), PipelineError> {
    // Build adjacency: step -> its needs
    let graph: HashMap<&str, Vec<&str>> = pf
        .steps
        .iter()
        .map(|(k, v)| (k.as_str(), v.needs.iter().map(String::as_str).collect()))
        .collect();

    let mut visited: HashSet<&str> = HashSet::new();
    let mut in_stack: HashSet<&str> = HashSet::new();

    fn dfs<'a>(
        node: &'a str,
        graph: &HashMap<&'a str, Vec<&'a str>>,
        visited: &mut HashSet<&'a str>,
        in_stack: &mut HashSet<&'a str>,
    ) -> Result<(), PipelineError> {
        if in_stack.contains(node) {
            return Err(PipelineError::CycleDetected(node.to_string()));
        }
        if visited.contains(node) {
            return Ok(());
        }
        in_stack.insert(node);
        for &dep in graph.get(node).unwrap_or(&vec![]) {
            dfs(dep, graph, visited, in_stack)?;
        }
        in_stack.remove(node);
        visited.insert(node);
        Ok(())
    }

    for &node in graph.keys() {
        dfs(node, &graph, &mut visited, &mut in_stack)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
version: 1
triggers:
  - on: push
    branches: [main]
steps:
  build:
    run: ci/build.dail
  test:
    run: ci/test.dail
    needs: [build]
"#;

    #[test]
    fn parses_valid_pipeline() {
        let pf = parse(VALID).unwrap();
        assert_eq!(pf.steps.len(), 2);
    }

    #[test]
    fn rejects_bad_version() {
        let yaml = VALID.replace("version: 1", "version: 2");
        assert!(matches!(
            parse(&yaml),
            Err(PipelineError::UnsupportedVersion(2))
        ));
    }

    #[test]
    fn rejects_unknown_dep() {
        let yaml = r#"
version: 1
steps:
  test:
    run: ci/test.dail
    needs: [ghost]
"#;
        assert!(matches!(
            parse(yaml),
            Err(PipelineError::UnknownDependency(_, _))
        ));
    }

    #[test]
    fn rejects_cycle() {
        let yaml = r#"
version: 1
steps:
  a:
    run: ci/a.dail
    needs: [b]
  b:
    run: ci/b.dail
    needs: [a]
"#;
        assert!(matches!(parse(yaml), Err(PipelineError::CycleDetected(_))));
    }

    #[test]
    fn parses_artifacts() {
        let yaml = r#"
version: 1
steps:
  build:
    run: ci/build.dail
    artifacts: [target/debug/flowz, dist/]
"#;
        let pf = parse(yaml).unwrap();
        assert_eq!(pf.steps["build"].artifacts.len(), 2);
    }

    #[test]
    fn rejects_bad_artifact_path() {
        let yaml = r#"
version: 1
steps:
  build:
    run: ci/build.dail
    artifacts: [../etc/passwd]
"#;
        assert!(matches!(
            parse(yaml),
            Err(PipelineError::InvalidArtifactPath { .. })
        ));
    }

    #[test]
    fn rejects_absolute_run_path() {
        let yaml = r#"
version: 1
steps:
  bad:
    run: /etc/passwd
"#;
        assert!(matches!(
            parse(yaml),
            Err(PipelineError::InvalidRunPath { .. })
        ));
    }
}

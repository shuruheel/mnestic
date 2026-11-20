//! Untimed input loading and qualification admission. This is a reproducibility
//! gate, not a security boundary against a caller who forges local evidence.
use super::*;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Input {
    path: PathBuf,
    sha256: String,
}
impl Input {
    pub(super) fn read(&self) -> Result<Value> {
        ensure!(self.path.is_absolute(), "input paths must be absolute");
        let bytes = fs::read(&self.path).into_diagnostic()?;
        ensure!(
            hash(&bytes) == self.sha256,
            "input hash changed: {}",
            self.path.display()
        );
        serde_json::from_slice(&bytes).into_diagnostic()
    }
}

pub(super) fn authorize(plan: &Plan, bytes: &[u8], executable_hash: &str) -> Result<Option<Value>> {
    ensure!(plan.version == 1, "unsupported plan version");
    if plan.purpose == "development" {
        ensure!(plan.seed == 99, "development requires seed 99");
        return Ok(None);
    }
    ensure!(
        plan.purpose == "qualification"
            && [1234, 1235, 1236].contains(&plan.seed)
            && ["sqlite", "rocksdb"].contains(&plan.backend.as_str()),
        "invalid qualification identity"
    );
    ensure!(
        plan.overhead_control.is_none(),
        "qualification cannot run development controls"
    );
    let path = std::env::var_os("MNESTIC_QUALIFICATION_MANIFEST")
        .ok_or_else(|| miette::miette!("qualification requires a frozen campaign manifest"))?;
    let proof_bytes = fs::read(&path).into_diagnostic()?;
    let value = validate_proof(plan, bytes, executable_hash, &proof_bytes)?;
    Ok(Some(
        json!({"path":path,"sha256":hash(&proof_bytes),"controls_manifest_sha256":value}),
    ))
}

fn validate_proof(
    plan: &Plan,
    bytes: &[u8],
    executable_hash: &str,
    proof_bytes: &[u8],
) -> Result<String> {
    let proof: Value = serde_json::from_slice(proof_bytes).into_diagnostic()?;
    ensure!(
        proof["version"] == 1 && proof["qualification_authorized"] == true,
        "campaign not authorized"
    );
    let identity = &proof["plans"][hash(bytes)];
    ensure!(
        identity["seed"] == plan.seed
            && identity["backend"] == plan.backend
            && identity["executable_sha256"] == executable_hash,
        "plan/binary absent from campaign freeze"
    );
    let completion_ref: Input =
        serde_json::from_value(proof["controls_completion"].clone()).into_diagnostic()?;
    let control_ref: Input =
        serde_json::from_value(proof["controls_manifest"].clone()).into_diagnostic()?;
    let completion = completion_ref.read()?;
    let controls = control_ref.read()?;
    ensure!(
        completion["all_recording_controls_passed"] == true
            && completion["phase"] == 3
            && controls["phase"] == 3
            && completion["completed_blocks"] == 180
            && completion["scripts_and_protocol_unchanged"] == true
            && completion["campaign_manifest_sha256"] == control_ref.sha256,
        "recording controls did not pass for the frozen manifest"
    );
    ensure!(
        controls["builds"].as_object().is_some_and(|builds| builds
            .values()
            .any(|b| b["executable_sha256"] == executable_hash)),
        "qualification binary differs from recording-control binaries"
    );
    for op in operations(plan)? {
        for input in [&op.params_file, &op.expected_rows_file]
            .into_iter()
            .flatten()
        {
            ensure!(
                proof["inputs"][input.path.to_string_lossy().as_ref()] == input.sha256,
                "external input absent from campaign freeze"
            );
        }
    }
    Ok(control_ref.sha256)
}

fn operations(plan: &Plan) -> Result<Vec<&Operation>> {
    let mut all: Vec<_> = plan.setup.iter().chain(&plan.verify).collect();
    for step in &plan.steps {
        match step {
            Step::Serial { operations, .. } => all.extend(operations),
            Step::Concurrent { workers, .. } => all.extend(workers.iter().flatten()),
            Step::Build {
                ddl,
                workers,
                diagnostic_drain_after_ms,
                ..
            } => {
                // A diagnostic drain can never be part of a qualifying plan.
                ensure!(
                    diagnostic_drain_after_ms.is_none(),
                    "qualification forbids diagnostic writer drains"
                );
                all.push(ddl.as_ref());
                all.extend(workers.iter().flatten());
            }
        }
    }
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frozen_file(path: &Path, value: Value) -> Value {
        let bytes = serde_json::to_vec(&value).unwrap();
        fs::write(path, &bytes).unwrap();
        json!({"path":path,"sha256":hash(&bytes)})
    }

    #[test]
    fn qualification_requires_matching_plan_binary_controls_and_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let data = frozen_file(&dir.path().join("params.json"), json!({"x":1}));
        let plan_value = json!({"version":1,"purpose":"qualification","seed":1234,"backend":"sqlite",
            "setup":[],"steps":[{"mode":"serial","cell":"read","operations":[{"id":"read","script":"?[x] <- [[$x]]","params_file":data}]}]});
        let bytes = serde_json::to_vec(&plan_value).unwrap();
        let plan: Plan = serde_json::from_slice(&bytes).unwrap();
        let controls = frozen_file(
            &dir.path().join("controls.json"),
            json!({"phase":3,"builds":{"candidate":{"executable_sha256":"binary"}}}),
        );
        let completion = frozen_file(
            &dir.path().join("completion.json"),
            json!({"all_recording_controls_passed":true,"phase":3,"completed_blocks":180,"scripts_and_protocol_unchanged":true,"campaign_manifest_sha256":controls["sha256"]}),
        );
        let mut proof = json!({"version":1,"qualification_authorized":true,"plans":{},"inputs":{},"controls_manifest":controls,"controls_completion":completion});
        proof["plans"][hash(&bytes)] =
            json!({"seed":1234,"backend":"sqlite","executable_sha256":"binary"});
        proof["inputs"][data["path"].as_str().unwrap()] = data["sha256"].clone();
        let encode = |p: &Value| serde_json::to_vec(p).unwrap();
        assert!(validate_proof(&plan, &bytes, "binary", &encode(&proof)).is_ok());
        assert!(validate_proof(&plan, &bytes, "other", &encode(&proof)).is_err());
        assert!(validate_proof(&plan, b"changed plan", "binary", &encode(&proof)).is_err());
        let mut missing = proof.clone();
        missing["inputs"] = json!({});
        assert!(validate_proof(&plan, &bytes, "binary", &encode(&missing)).is_err());
        proof["controls_completion"] = frozen_file(
            &dir.path().join("failed.json"),
            json!({"all_recording_controls_passed":false,"campaign_manifest_sha256":controls["sha256"]}),
        );
        assert!(validate_proof(&plan, &bytes, "binary", &encode(&proof)).is_err());
    }
}

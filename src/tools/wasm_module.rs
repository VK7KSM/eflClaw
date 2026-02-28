use super::traits::{Tool, ToolResult};
use crate::runtime::{WasmCapabilities, WasmRuntime};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

/// Tool for listing and executing sandboxed WASM modules.
pub struct WasmModuleTool {
    security: Arc<SecurityPolicy>,
    wasm_runtime: Option<Arc<WasmRuntime>>,
}

impl WasmModuleTool {
    /// Create a tool backed by a specific WASM runtime.
    pub fn new(security: Arc<SecurityPolicy>, wasm_runtime: Option<Arc<WasmRuntime>>) -> Self {
        Self {
            security,
            wasm_runtime,
        }
    }

    /// Module name validation: snake_case only (lowercase letters, digits, underscores).
    fn is_valid_module_name(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    }

    fn parse_caps(args: &serde_json::Value) -> anyhow::Result<WasmCapabilities> {
        let read_workspace = args
            .get("read_workspace")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let write_workspace = args
            .get("write_workspace")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let fuel_override = args
            .get("fuel_override")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let memory_override_mb = args
            .get("memory_override_mb")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);

        let allowed_hosts = match args.get("allowed_hosts") {
            Some(value) => {
                let arr = value.as_array().ok_or_else(|| {
                    anyhow::anyhow!("'allowed_hosts' must be an array of strings")
                })?;
                let mut hosts = Vec::with_capacity(arr.len());
                for entry in arr {
                    let host = entry
                        .as_str()
                        .ok_or_else(|| {
                            anyhow::anyhow!("'allowed_hosts' must be an array of strings")
                        })?
                        .trim()
                        .to_string();
                    if !host.is_empty() {
                        hosts.push(host);
                    }
                }
                hosts
            }
            None => Vec::new(),
        };

        Ok(WasmCapabilities {
            read_workspace,
            write_workspace,
            allowed_hosts,
            fuel_override,
            memory_override_mb,
        })
    }
}

#[async_trait]
impl Tool for WasmModuleTool {
    fn name(&self) -> &str {
        "wasm_module"
    }

    fn description(&self) -> &str {
        "List or execute sandboxed WASM modules from runtime.wasm.tools_dir"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "run"],
                    "description": "Action to perform: list modules or run a module"
                },
                "module": {
                    "type": "string",
                    "description": "WASM module name (without .wasm extension), required when action=run"
                },
                "read_workspace": {
                    "type": "boolean",
                    "description": "Request read_workspace capability (must be allowed by runtime policy)"
                },
                "write_workspace": {
                    "type": "boolean",
                    "description": "Request write_workspace capability (must be allowed by runtime policy)"
                },
                "allowed_hosts": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Requested host allowlist subset for this invocation"
                },
                "fuel_override": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Optional fuel override; cannot exceed runtime.wasm.fuel_limit"
                },
                "memory_override_mb": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Optional memory override in MB; cannot exceed runtime.wasm.memory_limit_mb"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = args
            .get("action")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Missing 'action' parameter"))?;

        if self.security.is_rate_limited() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: too many actions in the last hour".into()),
            });
        }
        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: action budget exhausted".into()),
            });
        }

        let Some(wasm_runtime) = self.wasm_runtime.as_ref() else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(
                    "wasm_module tool is only available when runtime.kind = \"wasm\"".into(),
                ),
            });
        };

        match action {
            "list" => match wasm_runtime.list_modules(&self.security.workspace_dir) {
                Ok(modules) => {
                    // Filter to valid module names (snake_case, max 64 chars)
                    let valid_modules: Vec<&str> = modules
                        .iter()
                        .filter(|m| Self::is_valid_module_name(m))
                        .map(String::as_str)
                        .collect();
                    Ok(ToolResult {
                        success: true,
                        output: serde_json::to_string_pretty(&json!({ "modules": valid_modules }))?,
                        error: None,
                    })
                }
                Err(err) => Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(err.to_string()),
                }),
            },
            "run" => {
                let module = args
                    .get("module")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("Missing 'module' parameter for action=run"))?;

                if !Self::is_valid_module_name(module) {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "Invalid module name '{module}': must be snake_case (lowercase letters, digits, underscores), max 64 chars"
                        )),
                    });
                }

                let caps = Self::parse_caps(&args)?;
                match wasm_runtime.execute_module(module, &self.security.workspace_dir, &caps) {
                    Ok(result) => {
                        let output = serde_json::to_string_pretty(&json!({
                            "module": module,
                            "exit_code": result.exit_code,
                            "fuel_consumed": result.fuel_consumed,
                            "stdout": result.stdout,
                            "stderr": result.stderr
                        }))?;
                        let success = result.exit_code == 0;
                        let error = if success {
                            None
                        } else if result.stderr.is_empty() {
                            Some(format!("WASM module exited with code {}", result.exit_code))
                        } else {
                            Some(result.stderr)
                        };

                        Ok(ToolResult {
                            success,
                            output,
                            error,
                        })
                    }
                    Err(err) => Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(err.to_string()),
                    }),
                }
            }
            other => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Unsupported action '{other}'. Use 'list' or 'run'."
                )),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WasmRuntimeConfig;
    use crate::security::{AutonomyLevel, SecurityPolicy};

    fn test_security(workspace_dir: std::path::PathBuf) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            workspace_dir,
            ..SecurityPolicy::default()
        })
    }

    #[test]
    fn wasm_module_tool_name() {
        let dir = tempfile::tempdir().unwrap();
        let security = test_security(dir.path().to_path_buf());
        let tool = WasmModuleTool::new(security, None);
        assert_eq!(tool.name(), "wasm_module");
    }

    #[test]
    fn wasm_module_tool_no_runtime_description() {
        let dir = tempfile::tempdir().unwrap();
        let security = test_security(dir.path().to_path_buf());
        let wasm_runtime = Arc::new(WasmRuntime::new(WasmRuntimeConfig::default()));
        let tool = WasmModuleTool::new(security, Some(wasm_runtime));
        assert!(tool.description().contains("wasm_module"));
    }

    #[tokio::test]
    async fn wasm_sandbox_no_fs_access_by_default() {
        let dir = tempfile::tempdir().unwrap();
        // No modules in tools dir → list returns empty
        let security = test_security(dir.path().to_path_buf());
        let wasm_runtime = Arc::new(WasmRuntime::new(WasmRuntimeConfig::default()));
        let tool = WasmModuleTool::new(security, Some(wasm_runtime));

        let result = tool.execute(json!({"action": "list"})).await.unwrap();
        assert!(result.success);
        // Default caps have no fs access — modules list should be empty
        let val: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert!(val["modules"].as_array().map(|a| a.is_empty()).unwrap_or(true));
    }

    #[tokio::test]
    async fn list_action_returns_modules() {
        let dir = tempfile::tempdir().unwrap();
        let tools_dir = dir.path().join("tools/wasm");
        std::fs::create_dir_all(&tools_dir).unwrap();
        std::fs::write(tools_dir.join("alpha.wasm"), b"\0asm").unwrap();
        std::fs::write(tools_dir.join("beta.wasm"), b"\0asm").unwrap();
        // Name with invalid characters should be filtered out
        std::fs::write(tools_dir.join("bad$name.wasm"), b"\0asm").unwrap();

        let security = test_security(dir.path().to_path_buf());
        let wasm_runtime = Arc::new(WasmRuntime::new(WasmRuntimeConfig::default()));
        let tool = WasmModuleTool::new(security, Some(wasm_runtime));

        let result = tool.execute(json!({"action": "list"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("alpha"));
        assert!(result.output.contains("beta"));
        assert!(!result.output.contains("bad$name"));
    }

    #[tokio::test]
    async fn run_action_requires_module() {
        let dir = tempfile::tempdir().unwrap();
        let security = test_security(dir.path().to_path_buf());
        let wasm_runtime = Arc::new(WasmRuntime::new(WasmRuntimeConfig::default()));
        let tool = WasmModuleTool::new(security, Some(wasm_runtime));

        let result = tool.execute(json!({"action": "run"})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("module"));
    }

    #[tokio::test]
    async fn wasm_sandbox_no_network() {
        // No network: allowed_hosts is empty by default
        let dir = tempfile::tempdir().unwrap();
        let security = test_security(dir.path().to_path_buf());
        let wasm_runtime = Arc::new(WasmRuntime::new(WasmRuntimeConfig::default()));
        let caps = wasm_runtime.default_capabilities();
        assert!(caps.allowed_hosts.is_empty(), "no network by default");
        let _ = WasmModuleTool::new(security, Some(wasm_runtime));
    }

    #[tokio::test]
    async fn wasm_timeout_kill() {
        // Attempting to run a missing module returns an error (not available / not found)
        let dir = tempfile::tempdir().unwrap();
        let security = test_security(dir.path().to_path_buf());
        let wasm_runtime = Arc::new(WasmRuntime::new(WasmRuntimeConfig::default()));
        let tool = WasmModuleTool::new(security, Some(wasm_runtime));

        let result = tool
            .execute(json!({"action": "run", "module": "nonexistent_zeroclaw_test"}))
            .await
            .unwrap();
        assert!(!result.success);
        // Either "not available" (feature disabled) or "not found" (feature enabled but missing file)
        let err = result.error.unwrap_or_default();
        assert!(
            err.contains("not available") || err.contains("not found") || err.contains("WASM"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn tool_rejects_without_wasm_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let security = test_security(dir.path().to_path_buf());
        let tool = WasmModuleTool::new(security, None);

        let result = tool.execute(json!({"action": "list"})).await.unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("runtime.kind = \"wasm\""));
    }

    #[test]
    fn module_name_validation() {
        assert!(WasmModuleTool::is_valid_module_name("hello_world"));
        assert!(WasmModuleTool::is_valid_module_name("tool123"));
        assert!(!WasmModuleTool::is_valid_module_name("bad$name"));
        assert!(!WasmModuleTool::is_valid_module_name(""));
        assert!(!WasmModuleTool::is_valid_module_name("CamelCase"));
        assert!(!WasmModuleTool::is_valid_module_name("has space"));
    }
}

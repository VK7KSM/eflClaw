use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};

/// Tool that returns the current system date, time, and timezone.
/// Models (especially Gemini) often hallucinate the current time;
/// this tool gives them a reliable source of truth.
pub struct GetCurrentTimeTool;

#[async_trait]
impl Tool for GetCurrentTimeTool {
    fn name(&self) -> &str {
        "get_current_time"
    }

    fn description(&self) -> &str {
        "获取当前精确的系统时间、日期、时区。当需要知道准确时间时必须调用此工具，不要猜测。"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
        let now = chrono::Local::now();
        let hostname = hostname::get()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        let output = format!(
            "当前时间: {}\n时区: {}\nUnix时间戳: {}\n星期: {}\n主机: {}\n系统: {}",
            now.format("%Y-%m-%d %H:%M:%S"),
            now.format("%:z (%Z)"),
            now.timestamp(),
            now.format("%A"),
            hostname,
            std::env::consts::OS,
        );
        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_current_time_returns_valid_output() {
        let tool = GetCurrentTimeTool;
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("当前时间:"));
        assert!(result.output.contains("Unix时间戳:"));
        assert!(result.output.contains("星期:"));
    }

    #[test]
    fn tool_metadata() {
        let tool = GetCurrentTimeTool;
        assert_eq!(tool.name(), "get_current_time");
        assert!(!tool.description().is_empty());
    }
}
